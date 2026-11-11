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
const SOURCE_ID: &str = "e23229df-4678-47d7-ad6f-6cb140ab0c50";
const SINK_ID: &str = "873b5ad5-677b-4c93-b4dc-5058e72e9b55";
const CALLER_ID: &str = "974ddbbe-a288-4c61-a3cd-388d9cf74c2e";

fn id(uuid: &str) -> ComponentId {
    ComponentId::try_new(uuid).expect("component UUID")
}

fn port(name: &str) -> PortId {
    PortId::try_new(name).expect("port id")
}

fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("nonzero capacity")
}

fn descriptor(uuid: &str, direction: PortDirection) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        id(uuid),
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
        Self::with_capacity(fail_start, 128).await
    }

    async fn with_capacity(fail_start: bool, capacity: usize) -> Self {
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
                capacity: size(capacity),
                durable: true,
                retention: RetentionPolicy::Backpressure,
                subscribers: BTreeMap::from([("sink".into(), SubscriptionStart::Earliest)]),
            },
            provider
                .create_indexes(GRAPH, SOURCE_ID)
                .await
                .expect("indexes"),
            codec,
            "ingress",
            QosRecoveryOptions::Admission(AdmissionOptions {
                construction_scope: INSTANCE.into(),
                graph_id: GRAPH.into(),
                component_id: id(SOURCE_ID),
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
            .source(
                ComponentName::try_new("source").expect("source name"),
                Box::new(Source {
                    descriptor: descriptor(SOURCE_ID, PortDirection::Output),
                    admission: admission.clone(),
                    fail_start,
                }),
            )
            .sink(
                ComponentName::try_new("sink").expect("sink name"),
                Box::new(Sink {
                    descriptor: descriptor(SINK_ID, PortDirection::Input),
                    received,
                    handled: handled.clone(),
                }),
            )
            .declare_resource(ResourceSpecification {
                id: resource.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: "ingress".into(),
            })
            .expect("declare journal")
            .provide_resource(resource.clone(), channel.resource())
            .expect("provide actual journal")
            .bind_stream(Endpoint::new(id(SOURCE_ID), port("out")), stream)
            .connect(
                EdgeDefinition {
                    from: Endpoint::new(id(SOURCE_ID), port("out")),
                    to: Endpoint::new(id(SINK_ID), port("in")),
                },
                Box::new(channel.definition().pipe(resource, "sink")),
            )
            .build()
            .expect("native graph");
        let core = DrasiLib::builder()
            .with_allow_external_component_ids(true)
            .with_query_factory(
                drasi_lib::computation::v1::FactoryRegistry::standard()
                    .get(
                        &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
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
        .register_producer(id(CALLER_ID))
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
    tokio::time::timeout(DEADLINE, async {
        loop {
            let progress = fixture
                .admission
                .channel()
                .progress()
                .await
                .expect("processed progress");
            if progress.processed.get("sink") == Some(&receipt.position) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("downstream handling acknowledged before restart");
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
        .stop_source_by_id(&id(SOURCE_ID))
        .await
        .expect("stop native source");
    assert!(!fixture.admission.is_active().expect("revoked"));
    assert!(matches!(
        fixture.admission.wait_active().await,
        Err(PipeError::Closed)
    ));
    assert!(matches!(
        fixture.admission.register_producer(id(CALLER_ID)).await,
        Err(PipeError::Closed)
    ));
    fixture
        .core
        .start_source_by_id(&id(SOURCE_ID))
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
            .register_producer(id(CALLER_ID))
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

async fn awaitable_admission_after_start() {
    use drasi_lib::observability::{ExecutionContext, ExecutionScope, TraceContextV1};

    let mut fixture = Fixture::with_capacity(false, 1).await;
    fixture.core.start().await.expect("host start");
    fixture.admission.wait_active().await.expect("mailbox");
    let session = fixture
        .admission
        .register_producer(id(CALLER_ID))
        .await
        .expect("session");
    let input = input();
    let trace = TraceContextV1::new([1; 16], [2; 8], 1, None).expect("trace context");
    assert_eq!(
        ExecutionScope::new(ExecutionContext::new(trace.clone()))
            .scope(fixture.admission.admit_wait(&session, 1, &input))
            .await
            .expect("first input")
            .position,
        1
    );
    let first = fixture
        .received
        .recv()
        .await
        .expect("first downstream input");
    assert_eq!(first.execution_context.trace(), Some(&trace));
    {
        let waiting = fixture.admission.admit_wait(&session, 2, &input);
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        // This later mailbox request proves the graph has processed the full
        // journal rejection and is not blocked waiting for subscriber capacity.
        assert_eq!(
            fixture
                .admission
                .producer_status(&session)
                .await
                .expect("status during backpressure")
                .next_sequence,
            Some(2)
        );
        assert!(futures::poll!(&mut waiting).is_pending());
        fixture.handled.add_permits(1);
        assert_eq!(
            waiting.await.expect("admit after acknowledgement").position,
            2
        );
    }
    let second = fixture
        .received
        .recv()
        .await
        .expect("second downstream input");
    assert!(second.execution_context.trace().is_none());
    assert_eq!(
        fixture
            .admission
            .admit_wait(&session, 2, &input)
            .await
            .expect("idempotent retry")
            .position,
        2
    );
    fixture.handled.add_permits(1);
    fixture.close().await;
}

#[tokio::test]
async fn awaitable_admission_backpressures_through_the_actual_graph() {
    tokio::time::timeout(DEADLINE, awaitable_admission_after_start())
        .await
        .expect("admission deadline");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn awaitable_admission_backpressures_on_multithread_runtime() {
    tokio::time::timeout(DEADLINE, awaitable_admission_after_start())
        .await
        .expect("admission deadline");
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
