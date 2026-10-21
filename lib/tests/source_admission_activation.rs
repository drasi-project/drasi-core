// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::BTreeMap, num::NonZeroUsize, sync::Arc, time::Duration};

use async_trait::async_trait;
use drasi_core::{
    computation::ComputationIndexProvider,
    interface::FailureMode,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{computation::v1::*, DrasiLib};
use tokio::sync::{mpsc, Semaphore};

const GRAPH: &str = "__drasi_lib_runtime__";
const INSTANCE: &str = "admission-activation";
const DEADLINE: Duration = Duration::from_secs(10);

fn id(name: &str) -> ComponentId {
    ComponentId::try_new(name).expect("component id")
}

fn port(name: &str) -> PortId {
    PortId::try_new(name).expect("port id")
}

fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("nonzero capacity")
}

fn descriptor(name: &str, direction: PortDirection) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        id(name),
        vec![PortDescriptor::new(
            port(if direction == PortDirection::Output {
                "out"
            } else {
                "in"
            }),
            direction,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )
    .expect("descriptor")
}

struct Source {
    descriptor: ComponentDescriptor,
    admission: Arc<SourceAdmission>,
    fail_start: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("activation test source failed to start")]
struct StartFailure;

#[async_trait]
impl ComputationComponent for Source {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        if self.fail_start {
            return Err(StartFailure.into());
        }
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for Source {
    fn admission(&self) -> Option<Arc<SourceAdmission>> {
        Some(self.admission.clone())
    }
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        anyhow::bail!("admitted sources must use the graph mailbox")
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    received: mpsc::UnboundedSender<InputEnvelope>,
    handled: Arc<Semaphore>,
}

#[async_trait]
impl ComputationComponent for Sink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.received.send(input)?;
        self.handled.acquire().await?.forget();
        Ok(())
    }
}

struct Fixture {
    core: DrasiLib,
    admission: Arc<SourceAdmission>,
    received: mpsc::UnboundedReceiver<InputEnvelope>,
    handled: Arc<Semaphore>,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new(fail_start: bool) -> Self {
        let directory = tempfile::tempdir().expect("store directory");
        let provider = LegacyIndexProviderAdapter::scoped(
            Arc::new(
                RocksDbIndexProvider::new(directory.path(), false, false)
                    .with_memory_budget_bytes(32 << 20)
                    .expect("bounded storage"),
            ),
            INSTANCE,
        )
        .expect("scoped provider");
        let stream = StreamId::try_new("source/out").expect("stream");
        let mut codec = EnvelopeCodec::new(size(64 * 1024));
        codec
            .register_schema(GraphChangeCodec::schema())
            .expect("schema");
        let channel = QosChannel::persistent_with_recovery(
            QosChannelDefinition {
                stream: stream.clone(),
                capacity: size(128),
                durable: true,
                retention: RetentionPolicy::Backpressure,
                subscribers: BTreeMap::from([("sink".into(), SubscriptionStart::Earliest)]),
            },
            provider
                .create_indexes(GRAPH, "ingress")
                .await
                .expect("indexes"),
            codec,
            "ingress",
            QosRecoveryOptions::Admission(AdmissionOptions {
                construction_scope: INSTANCE.into(),
                graph_id: GRAPH.into(),
                component_id: id("source"),
                failure_scope: FailureMode::ProcessRestart,
                max_producers: size(1),
                receipts_per_producer: size(128),
            }),
        )
        .await
        .expect("durable admission");
        let admission = SourceAdmission::new(channel.clone(), port("out"))
            .await
            .expect("admission service");
        let resource = ResourceId::try_new("ingress").expect("resource");
        let (received, receive) = mpsc::unbounded_channel();
        let handled = Arc::new(Semaphore::new(0));
        let batch = ComponentBatch::builder()
            .source(Box::new(Source {
                descriptor: descriptor("source", PortDirection::Output),
                admission: admission.clone(),
                fail_start,
            }))
            .sink(Box::new(Sink {
                descriptor: descriptor("sink", PortDirection::Input),
                received,
                handled: handled.clone(),
            }))
            .declare_resource(ResourceSpecification {
                id: resource.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: "ingress".into(),
            })
            .expect("declare journal")
            .provide_resource(resource.clone(), channel.resource())
            .expect("provide actual journal")
            .bind_stream(Endpoint::new(id("source"), port("out")), stream)
            .connect(
                EdgeDefinition {
                    from: Endpoint::new(id("source"), port("out")),
                    to: Endpoint::new(id("sink"), port("in")),
                },
                Box::new(channel.definition().pipe(resource, "sink")),
            )
            .build()
            .expect("native graph");
        let core = DrasiLib::builder()
            .with_id(INSTANCE)
            .with_components(batch)
            .build()
            .await
            .expect("host");
        Self {
            core,
            admission,
            received: receive,
            handled,
            _directory: directory,
        }
    }

    async fn close(self) {
        self.core.shutdown().await.expect("host shutdown");
        self.admission
            .channel()
            .shutdown()
            .await
            .expect("borrowed journal shutdown");
    }
}

fn input() -> ChangeEnvelope {
    GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", "one"),
                    labels: vec!["Item".into()].into(),
                    effective_from: 1,
                },
                properties: ElementPropertyMap::new(),
            },
        },
        StreamId::try_new("source/out").expect("stream"),
        1,
        None,
    )
    .expect("input")
}

async fn real_mailbox_after_start() {
    let mut fixture = Fixture::new(false).await;
    fixture.core.start().await.expect("host start");
    fixture
        .admission
        .wait_active()
        .await
        .expect("actual mailbox");
    assert!(fixture.admission.is_active().expect("active"));
    let session = fixture
        .admission
        .register_producer(id("caller"))
        .await
        .expect("immediate registration");
    let input = input();
    let receipt = fixture
        .admission
        .admit(&session, 1, &input)
        .await
        .expect("immediate admission");
    assert_eq!(receipt.position, 1);
    fixture
        .received
        .recv()
        .await
        .expect("real downstream input");
    assert_eq!(
        fixture
            .admission
            .admit(&session, 1, &input)
            .await
            .expect("exact retry"),
        receipt
    );
    fixture.handled.add_permits(1);
    assert_eq!(
        fixture
            .admission
            .channel()
            .progress()
            .await
            .expect("progress")
            .accepted,
        1
    );
    fixture
        .core
        .stop_source("source")
        .await
        .expect("stop native source");
    assert!(!fixture.admission.is_active().expect("revoked"));
    assert!(matches!(
        fixture.admission.wait_active().await,
        Err(PipeError::Closed)
    ));
    assert!(matches!(
        fixture.admission.register_producer(id("caller")).await,
        Err(PipeError::Closed)
    ));
    fixture
        .core
        .start_source("source")
        .await
        .expect("restart native source");
    fixture
        .admission
        .wait_active()
        .await
        .expect("new activation");
    assert_eq!(
        fixture
            .admission
            .register_producer(id("caller"))
            .await
            .expect("same durable session"),
        session
    );
    assert_eq!(
        fixture
            .admission
            .admit(&session, 1, &input)
            .await
            .expect("retry after restart"),
        receipt
    );
    assert_eq!(
        fixture
            .admission
            .channel()
            .progress()
            .await
            .expect("progress after restart")
            .accepted,
        1
    );
    fixture.close().await;
}

#[tokio::test]
async fn fence_precedes_first_registration_and_preserves_restart_deduplication() {
    tokio::time::timeout(DEADLINE, real_mailbox_after_start())
        .await
        .expect("activation deadline");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fence_and_actual_admission_work_on_multithread_runtime() {
    tokio::time::timeout(DEADLINE, real_mailbox_after_start())
        .await
        .expect("activation deadline");
}

#[tokio::test]
async fn failed_native_start_is_not_activation_success() {
    tokio::time::timeout(DEADLINE, async {
        let fixture = Fixture::new(true).await;
        fixture
            .core
            .start()
            .await
            .expect_err("source start must fail");
        let PipeError::Backend(error) = fixture
            .admission
            .wait_active()
            .await
            .expect_err("failed activation")
        else {
            panic!("expected retained graph failure");
        };
        let graph = error
            .downcast_ref::<GraphError>()
            .expect("typed graph failure");
        let GraphError::Component { source, .. } = graph.underlying() else {
            panic!("expected component start failure");
        };
        assert!(source.downcast_ref::<StartFailure>().is_some());
        assert!(!fixture.admission.is_active().expect("inactive"));
        fixture.close().await;
    })
    .await
    .expect("failure deadline");
}
