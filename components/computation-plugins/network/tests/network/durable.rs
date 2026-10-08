// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use async_trait::async_trait;
use drasi_computation_network::proto::admission as proto;
use drasi_core::{computation::ComputationIndexProvider, interface::FailureMode};
use drasi_index_rocksdb::RocksDbIndexProvider;
use std::{collections::BTreeMap, future::Future, num::NonZeroUsize};

struct CommitGate {
    armed: std::sync::atomic::AtomicBool,
    before: bool,
    entered: Notify,
    resume: Notify,
}
struct GatedSession {
    inner: Arc<dyn drasi_core::interface::SessionControl>,
    gate: Arc<CommitGate>,
}
#[async_trait]
impl drasi_core::interface::SessionControl for GatedSession {
    async fn begin(&self) -> std::result::Result<(), drasi_core::interface::IndexError> {
        self.inner.begin().await
    }
    async fn commit(&self) -> std::result::Result<(), drasi_core::interface::IndexError> {
        let armed = self.gate.armed.swap(false, Ordering::AcqRel);
        if armed && self.gate.before {
            self.gate.entered.notify_one();
            self.gate.resume.notified().await;
        }
        self.inner.commit().await?;
        if armed && !self.gate.before {
            self.gate.entered.notify_one();
            self.gate.resume.notified().await;
        }
        Ok(())
    }
    fn rollback(&self) -> std::result::Result<(), drasi_core::interface::IndexError> {
        self.inner.rollback()
    }
}
fn gated_indexes(
    original: drasi_core::computation::ComputationIndexes,
    gate: Arc<CommitGate>,
) -> Result<drasi_core::computation::ComputationIndexes> {
    use drasi_core::{
        computation::{ComputationIndexes, ComputationResource, TransactionDomain},
        interface::IndexSet,
    };
    let control: Arc<dyn drasi_core::interface::SessionControl> = Arc::new(GatedSession {
        inner: original.indexes().session_control.clone(),
        gate,
    });
    let domain = TransactionDomain::new(control.clone());
    let set = original.indexes();
    Ok(ComputationIndexes::try_new(
        IndexSet {
            element_index: set.element_index.clone(),
            archive_index: set.archive_index.clone(),
            result_index: set.result_index.clone(),
            future_queue: set.future_queue.clone(),
            session_control: control,
        },
        Some(domain.clone()),
        Some(ComputationResource::participating(
            original
                .checkpoint_store()
                .expect("checkpoint store")
                .clone(),
            &domain,
        )),
        Some(ComputationResource::participating(
            original.outbox_writer().expect("outbox").clone(),
            &domain,
        )),
        Some(ComputationResource::participating(
            original
                .live_results_writer()
                .expect("live results")
                .clone(),
            &domain,
        )),
    )?
    .with_cleanup(original.cleanup().expect("cleanup").clone())
    .with_durability(original.durability()))
}

#[tokio::test(flavor = "current_thread")]
async fn network_timeouts_and_source_stop_resolve_real_commits_without_duplicate_acceptance(
) -> Result<()> {
    for grpc in [false, true] {
        for before in [false, true] {
            for stop in [false, true] {
                let directory = tempfile::tempdir()?;
                let gate = Arc::new(CommitGate {
                    armed: std::sync::atomic::AtomicBool::new(false),
                    before,
                    entered: Notify::new(),
                    resume: Notify::new(),
                });
                let port = free_port()?;
                let mut harness = Harness::with_gate(
                    directory.path(),
                    port,
                    grpc,
                    Some(gate.clone()),
                    "facilities-db",
                )
                .await?;
                let channel = harness.channel.clone();
                let saved = harness.drive(false, |control| {
                    let gate = gate.clone();
                    let channel = channel.clone();
                    async move {
                    let mut saved = None;
                    let send = async {
                        if grpc {
                            let mut client = Client::connect(format!("http://127.0.0.1:{port}")).await?;
                            let session = register(&mut client).await?;
                            saved = Some(ProducerSession { incarnation: session.incarnation.parse()?, producer: ComponentId::try_new(session.producer.clone())?, epoch: session.epoch });
                            gate.armed.store(true, Ordering::Release);
                            match client.admit(request(&session, 1, 1)).await {
                                Ok(response) => assert!(matches!(response.into_inner().result,
                                    Some(proto::admit_response::Result::Error(error)) if error.kind == proto::ErrorKind::AcceptanceUnknown as i32)),
                                Err(error) => assert!([tonic::Code::Cancelled, tonic::Code::DeadlineExceeded, tonic::Code::Unavailable].contains(&error.code()), "{error:?}"),
                            }
                        } else {
                            let client = reqwest::Client::builder().timeout(DEADLINE).build()?;
                            let base = format!("http://127.0.0.1:{port}/sources/facilities-db/admission/v1");
                            let (_, session) = post(&client, &base, "producers/register", json!({"producer":"client"})).await?;
                            saved = Some(serde_json::from_value::<ProducerSession>(session.clone())?);
                            gate.armed.store(true, Ordering::Release);
                            let (_, outcome) = post(&client, &base, "events", json!({"session":session,"sequence":1,"event":http_event(1)})).await?;
                            assert_eq!(outcome["error"]["kind"], "AcceptanceUnknown", "{outcome}");
                            assert_eq!(outcome["receipts"], json!([]));
                        }
                        Ok::<(), anyhow::Error>(())
                    };
                    let interrupt = async {
                        gate.entered.notified().await;
                        if stop {
                            let report = control.stop_components(GraphRevision(1), GraphSelection::Exact(vec![ComponentId::try_new("facilities-db")?])).await?;
                            assert_eq!(report.summary, OperationSummary::Completed, "{report:?}");
                        }
                        Ok::<(), anyhow::Error>(())
                    };
                    let (sent, interrupted) = tokio::join!(send, interrupt);
                    sent?;
                    interrupted?;
                    if stop {
                        assert!(channel.progress().await.is_err(), "interrupted commit must fence its owner");
                    } else {
                        gate.resume.notify_one();
                        assert_eq!(channel.progress().await?.accepted, 1, "client timeout does not roll back graph work");
                    }
                    saved.context("registered client session")
                }}).await?;
                drop(channel);
                harness.shutdown().await?;
                let port = free_port()?;
                let mut harness = Harness::new(directory.path(), port, grpc).await?;
                assert_eq!(
                    harness.channel.progress().await?.accepted,
                    u64::from(!stop || !before),
                    "grpc={grpc} before={before} stop={stop}"
                );
                let channel = harness.channel.clone();
                let seen = harness.seen.clone();
                harness.drive(true, |_| async {
                    if grpc {
                        let mut client = Client::connect(format!("http://127.0.0.1:{port}")).await?;
                        let session: proto::ProducerSession = saved.into();
                        assert_eq!(register(&mut client).await?, session);
                        assert!(matches!(admit(&mut client, request(&session, 1, 1)).await?, proto::admit_response::Result::Receipt(value) if value.position == 1));
                    } else {
                        let client = reqwest::Client::new();
                        let base = format!("http://127.0.0.1:{port}/sources/facilities-db/admission/v1");
                        let (_, session) = post(&client, &base, "producers/register", json!({"producer":"client"})).await?;
                        assert_eq!(serde_json::from_value::<ProducerSession>(session.clone())?, saved);
                        let (_, outcome) = post(&client, &base, "events", json!({"session":session,"sequence":1,"event":http_event(1)})).await?;
                        assert_eq!(outcome["receipts"][0]["position"], 1, "{outcome}");
                    }
                    while channel.progress().await?.processed["consumer"] != 1 { tokio::task::yield_now().await; }
                    assert_eq!(channel.progress().await?.accepted, 1);
                    assert_eq!(*seen.lock().unwrap(), [1]);
                    Ok(())
                }).await?;
                drop(channel);
                harness.shutdown().await?;
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_network_admission_crash_child() -> Result<()> {
    let Some(path) = std::env::var_os("DRASI_NETWORK_ADMISSION_CRASH") else {
        return Ok(());
    };
    let path = PathBuf::from(path);
    let grpc = std::env::var("DRASI_NETWORK_ADMISSION_GRPC")? == "1";
    let port = free_port()?;
    let mut harness = Harness::new(&path, port, grpc).await?;
    harness.drive(false, |_| async {
        let saved;
        if grpc {
            let mut client = Client::connect(format!("http://127.0.0.1:{port}")).await?;
            let session = register(&mut client).await?;
            saved = json!({"incarnation":session.incarnation,"producer":session.producer,"epoch":session.epoch});
            std::fs::write(path.join("client-session.json"), serde_json::to_vec(&saved)?)?;
            assert!(matches!(admit(&mut client, request(&session, 1, 1)).await?, proto::admit_response::Result::Receipt(value) if value.position == 1));
        } else {
            let client = reqwest::Client::new();
            let base = format!("http://127.0.0.1:{port}/sources/facilities-db/admission/v1");
            let (_, session) = post(&client, &base, "producers/register", json!({"producer":"client"})).await?;
            saved = session.clone();
            std::fs::write(path.join("client-session.json"), serde_json::to_vec(&saved)?)?;
            let (_, outcome) = post(&client, &base, "events", json!({"session":session,"sequence":1,"event":http_event(1)})).await?;
            assert_eq!(outcome["receipts"][0]["position"], 1, "{outcome}");
        }
        // Neither the server nor the client runs cleanup, and the client never
        // records its receipt. Only the session and intended next input survive.
        std::process::exit(94)
    }).await
}

#[tokio::test(flavor = "current_thread")]
async fn both_native_network_protocols_recover_after_process_exit_without_client_receipt(
) -> Result<()> {
    for grpc in [false, true] {
        let directory = tempfile::tempdir()?;
        let output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "durable_tests::native_network_admission_crash_child",
                "--nocapture",
            ])
            .env("DRASI_NETWORK_ADMISSION_CRASH", directory.path())
            .env("DRASI_NETWORK_ADMISSION_GRPC", if grpc { "1" } else { "0" })
            .output()?;
        assert_eq!(
            output.status.code(),
            Some(94),
            "child did not reach its crash point:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let saved: ProducerSession = serde_json::from_slice(&std::fs::read(
            directory.path().join("client-session.json"),
        )?)?;
        let port = free_port()?;
        let mut harness = Harness::new(directory.path(), port, grpc).await?;
        let channel = harness.channel.clone();
        let seen = harness.seen.clone();
        harness.drive(true, |_| async {
            if grpc {
                let mut client = Client::connect(format!("http://127.0.0.1:{port}")).await?;
                let session: proto::ProducerSession = saved.into();
                assert_eq!(register(&mut client).await?, session);
                assert!(matches!(admit(&mut client, request(&session, 1, 1)).await?, proto::admit_response::Result::Receipt(value) if value.position == 1));
            } else {
                let client = reqwest::Client::new();
                let base = format!("http://127.0.0.1:{port}/sources/facilities-db/admission/v1");
                let (_, session) = post(&client, &base, "producers/register", json!({"producer":"client"})).await?;
                assert_eq!(serde_json::from_value::<ProducerSession>(session.clone())?, saved);
                let (_, outcome) = post(&client, &base, "events", json!({"session":session,"sequence":1,"event":http_event(1)})).await?;
                assert_eq!(outcome["receipts"][0]["position"], 1, "{outcome}");
            }
            while channel.progress().await?.processed["consumer"] != 1 { tokio::task::yield_now().await; }
            assert_eq!(channel.progress().await?.accepted, 1);
            assert_eq!(*seen.lock().unwrap(), [1]);
            Ok(())
        }).await?;
        drop(channel);
        harness.shutdown().await?;
    }
    Ok(())
}

struct Capture {
    descriptor: ComponentDescriptor,
    seen: Arc<Mutex<Vec<i64>>>,
}
#[async_trait]
impl ComputationComponent for Capture {
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
impl EnvelopeSink for Capture {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        assert!(GraphProducerProgress::from_envelope(&input.envelope)?
            .context("producer")?
            .identity()
            .persistent());
        self.seen
            .lock()
            .expect("capture lock")
            .push(source_value(&input.envelope)?);
        Ok(())
    }
}
struct Harness {
    graph: ComputationGraph,
    channel: Arc<QosChannel>,
    seen: Arc<Mutex<Vec<i64>>>,
}
impl Harness {
    async fn new(path: &Path, port: u16, grpc: bool) -> Result<Self> {
        Self::with_gate(path, port, grpc, None, "facilities-db").await
    }
    async fn with_gate(
        path: &Path,
        port: u16,
        grpc: bool,
        gate: Option<Arc<CommitGate>>,
        configured_stream: &str,
    ) -> Result<Self> {
        let indexes = LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(
            path, false, false,
        )));
        let definition = QosChannelDefinition {
            stream: stream("facilities-db"),
            capacity: NonZeroUsize::new(2).expect("capacity"),
            durable: true,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([("consumer".into(), SubscriptionStart::Earliest)]),
        };
        let original = indexes.create_indexes("durable-network", "journal").await?;
        let indexes = match &gate {
            Some(gate) => gated_indexes(original, gate.clone())?,
            None => original,
        };
        let channel = QosChannel::persistent(
            definition.clone(),
            indexes,
            FactoryRegistry::standard()
                .envelope_codec(NonZeroUsize::new(1024 * 1024).expect("codec limit"))?,
            "journal",
        )
        .await?;
        channel
            .enable_admission(AdmissionOptions {
                construction_scope: "durable-network".into(),
                graph_id: "durable-network".into(),
                component_id: ComponentId::try_new("facilities-db")?,
                failure_scope: FailureMode::ProcessRestart,
                max_producers: NonZeroUsize::new(2).expect("producer limit"),
                receipts_per_producer: NonZeroUsize::new(2).expect("receipt limit"),
            })
            .await?;
        let plugin = plugin()?;
        let factory = plugin
            .factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref()
                    == if grpc { GRPC_SOURCE } else { HTTP_SOURCE }
            })
            .context("source factory")?
            .clone();
        let resource = ResourceId::try_new("journal")?;
        let mut specification = factory.specification(ComponentId::try_new("facilities-db")?,
            json!({"stream":configured_stream,"host":"127.0.0.1","port":port,"timeoutMs":if gate.is_some() { 100 } else { 2000 },"ingressCapacity":4}))?;
        specification
            .dependencies
            .insert("admission".into(), vec![resource.clone()]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let source = Endpoint::new(
            ComponentId::try_new("facilities-db")?,
            PortId::try_new("out")?,
        );
        let target = Endpoint::new(ComponentId::try_new("consumer")?, PortId::try_new("in")?);
        let graph = ComputationGraph::builder("durable-network")
            .declare_resource(ResourceSpecification {
                id: resource.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: "journal".into(),
            })?
            .provide_resource(resource.clone(), channel.resource())?
            .component(specification, factory)
            .sink(Box::new(Capture {
                descriptor: ComponentDescriptor::try_new(
                    ComponentId::try_new("consumer")?,
                    vec![PortDescriptor::new(
                        PortId::try_new("in")?,
                        PortDirection::Input,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                seen: seen.clone(),
            }))
            .bind_stream(source.clone(), definition.stream.clone())
            .connect(
                EdgeDefinition::new(source, target),
                Box::new(definition.pipe(resource, "consumer")),
            )
            .build()?;
        Ok(Self {
            graph,
            channel,
            seen,
        })
    }
    async fn drive<F, W, T>(&mut self, consumers: bool, work: F) -> Result<T>
    where
        F: FnOnce(GraphControl) -> W,
        W: Future<Output = Result<T>>,
    {
        let run = self.graph.run()?;
        let control = run.control();
        let client = async {
            let result = async {
                assert_eq!(
                    control.deployment_report().await?.summary,
                    OperationSummary::Completed
                );
                let selection = if consumers {
                    GraphSelection::All
                } else {
                    GraphSelection::Exact(vec![ComponentId::try_new("facilities-db")?])
                };
                let report = control
                    .start_components(GraphRevision(1), selection)
                    .await?;
                assert_eq!(report.summary, OperationSummary::Completed, "{report:?}");
                work(control.clone()).await
            }
            .await;
            control.cancel();
            result
        };
        let (run, result) =
            tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(run, client) })
                .await?;
        assert!(matches!(run, Err(GraphError::Cancelled)), "{run:?}");
        result
    }
    async fn shutdown(mut self) -> Result<()> {
        self.graph.shutdown().await?;
        drop(self.graph);
        self.channel.shutdown().await?;
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn durable_source_rejects_configured_stream_mismatch_before_listening() -> Result<()> {
    for grpc in [false, true] {
        let directory = tempfile::tempdir()?;
        let port = free_port()?;
        let mut harness =
            Harness::with_gate(directory.path(), port, grpc, None, "wrong-stream").await?;
        let run = harness.graph.run()?;
        let control = run.control();
        let inspect = async {
            let report = control.deployment_report().await;
            control.cancel();
            report
        };
        let (run, report) =
            tokio::time::timeout(DEADLINE, async { tokio::join!(run, inspect) }).await?;
        assert!(matches!(run, Err(GraphError::Cancelled)), "{run:?}");
        let report = report?;
        assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
        let failure = &report.components[&ComponentId::try_new("facilities-db")?];
        assert!(
            matches!(failure, CreationOutcome::CreationFailed(_)),
            "{failure:?}"
        );
        assert!(
            format!("{failure:?}").contains("native source stream does not match"),
            "{failure:?}"
        );
        assert_eq!(harness.channel.progress().await?.accepted, 0);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        drop(listener);
        harness.shutdown().await?;
    }
    Ok(())
}

async fn post(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    body: Value,
) -> Result<(StatusCode, Value)> {
    loop {
        let response = client
            .post(format!("{base}/{path}"))
            .json(&body)
            .send()
            .await?;
        let code = response.status();
        let body: Value = response.json().await?;
        if body["error"]["kind"] == "Busy" || body["error"]["kind"] == "Closed" {
            tokio::task::yield_now().await;
        } else {
            return Ok((code, body));
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn http_durable_receipts_partial_batch_reconstruction_and_retirement() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut saved_session = Value::Null;
    let mut saved_receipts = Value::Null;
    for reopened in [false, true] {
        let port = free_port()?;
        let mut harness = Harness::new(directory.path(), port, false).await?;
        let channel = harness.channel.clone();
        let seen = harness.seen.clone();
        let (session, receipts) = harness.drive(reopened, |_| async {
            let client = reqwest::Client::builder().timeout(DEADLINE).build()?;
            let base = format!("http://127.0.0.1:{port}/sources/facilities-db/admission/v1");
            let (code, session) = post(&client, &base, "producers/register", json!({"producer":"client"})).await?;
            assert_eq!(code, StatusCode::OK, "{session}");
            let (_, duplicate) = post(&client, &base, "producers/register", json!({"producer":"client"})).await?;
            assert_eq!(session, duplicate);
            let legacy = client.post(format!("http://127.0.0.1:{port}/sources/facilities-db/events"))
                .json(&http_event(99)).send().await?;
            assert_eq!(legacy.status(), StatusCode::CONFLICT);
            if !reopened {
                let mut event = http_event(1);
                event.as_object_mut().unwrap().remove("timestamp");
                let (code, _) = post(&client, &base, "events", json!({"session":session,"sequence":1,"event":event})).await?;
                assert_eq!(code, StatusCode::BAD_REQUEST);
                let (code, _) = post(&client, &base, "events/batch",
                    json!({"session":session,"first_sequence":1,"events":[http_event(1),{"operation":"delete","timestamp":1,"id":""}]})).await?;
                assert_eq!(code, StatusCode::BAD_REQUEST);
                assert_eq!(channel.progress().await?.accepted, 0);
                let (code, outcome) = post(&client, &base, "events/batch",
                    json!({"session":session,"first_sequence":1,"events":[http_event(1),http_event(2),http_event(3)]})).await?;
                assert_eq!(code, StatusCode::TOO_MANY_REQUESTS, "{outcome}");
                assert_eq!(outcome["error"]["kind"], "CapacityExhausted");
                assert_eq!(outcome["receipts"].as_array().unwrap().len(), 2);
                assert_eq!(channel.progress().await?.accepted, 2);
                assert!(seen.lock().unwrap().is_empty());
                let (_, receipt) = post(&client, &base, "receipts", json!({"session":session,"sequence":3})).await?;
                assert!(receipt.is_null());
                let (code, outcome) = post(&client, &base, "producers/retire", json!({"session":session})).await?;
                assert_eq!(code, StatusCode::CONFLICT);
                assert_eq!(outcome["error"]["kind"], "PendingObligations");
                let (_, retry) = post(&client, &base, "events", json!({"session":session,"sequence":1,"event":http_event(1)})).await?;
                assert_eq!(retry["receipts"][0]["position"], 1);
                let (_, receipts) = post(&client, &base, "events/batch",
                    json!({"session":session,"first_sequence":1,"events":[http_event(1),http_event(2)]})).await?;
                return Ok((session, receipts["receipts"].clone()));
            }
            assert_eq!(session, saved_session);
            while channel.progress().await?.processed["consumer"] != 2 { tokio::task::yield_now().await; }
            let (_, retry) = post(&client, &base, "events/batch",
                json!({"session":session,"first_sequence":1,"events":[http_event(1),http_event(2)]})).await?;
            assert_eq!(retry["receipts"], saved_receipts);
            assert!(retry["error"].is_null(), "{retry}");
            let (code, conflict) = post(&client, &base, "events",
                json!({"session":session,"sequence":2,"event":http_event(20)})).await?;
            assert_eq!(code, StatusCode::CONFLICT);
            assert_eq!(conflict["error"]["kind"], "PayloadConflict");
            let (code, outcome) = post(&client, &base, "events", json!({"session":session,"sequence":3,"event":http_event(3)})).await?;
            assert_eq!(code, StatusCode::OK, "{outcome}");
            assert_eq!(outcome["receipts"][0]["position"], 3);
            while channel.progress().await?.processed["consumer"] != 3 { tokio::task::yield_now().await; }
            assert_eq!(*seen.lock().unwrap(), [1,2,3]);
            let (_, status) = post(&client, &base, "producers/status", json!({"session":session})).await?;
            assert_eq!(status["next_sequence"], 4);
            assert_eq!(status["earliest_receipt"], 2);
            let (code, expired) = post(&client, &base, "events", json!({"session":session,"sequence":1,"event":http_event(1)})).await?;
            assert_eq!(code, StatusCode::GONE);
            assert_eq!(expired["error"]["kind"], "ReceiptExpired");
            assert_eq!(channel.progress().await?.accepted, 3);
            let (_, retired) = post(&client, &base, "producers/retire", json!({"session":session})).await?;
            assert_eq!(retired["retired"], true);
            let (_, replacement) = post(&client, &base, "producers/register", json!({"producer":"client"})).await?;
            assert_ne!(replacement["epoch"], session["epoch"]);
            Ok((session, saved_receipts.clone()))
        }).await?;
        saved_session = session;
        saved_receipts = receipts;
        drop(channel);
        harness.shutdown().await?;
    }
    Ok(())
}

type Client = proto::admission_service_client::AdmissionServiceClient<tonic::transport::Channel>;
async fn register(client: &mut Client) -> Result<proto::ProducerSession> {
    loop {
        match client
            .register_producer(proto::RegisterRequest {
                source_id: "facilities-db".into(),
                producer: "client".into(),
            })
            .await?
            .into_inner()
            .result
            .context("registration result")?
        {
            proto::register_response::Result::Session(session) => return Ok(session),
            proto::register_response::Result::Error(error)
                if [
                    proto::ErrorKind::Busy as i32,
                    proto::ErrorKind::Closed as i32,
                ]
                .contains(&error.kind) =>
            {
                tokio::task::yield_now().await
            }
            result => anyhow::bail!("unexpected registration: {result:?}"),
        }
    }
}
fn request(session: &proto::ProducerSession, sequence: u64, value: i64) -> proto::AdmitRequest {
    proto::AdmitRequest {
        source_id: "facilities-db".into(),
        session: Some(session.clone()),
        sequence,
        event: Some(proto_event(value)),
    }
}
async fn admit(
    client: &mut Client,
    request: proto::AdmitRequest,
) -> Result<proto::admit_response::Result> {
    loop {
        let result = client
            .admit(request.clone())
            .await?
            .into_inner()
            .result
            .context("admission result")?;
        match &result {
            proto::admit_response::Result::Error(error)
                if error.kind == proto::ErrorKind::Busy as i32 =>
            {
                tokio::task::yield_now().await
            }
            _ => return Ok(result),
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_durable_stream_reconstruction_receipts_and_retirement() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut saved = None;
    for reopened in [false, true] {
        let port = free_port()?;
        let mut harness = Harness::new(directory.path(), port, true).await?;
        let channel = harness.channel.clone();
        let seen = harness.seen.clone();
        let session = harness.drive(reopened, |_| async {
            let address = format!("http://127.0.0.1:{port}");
            let mut client = Client::connect(address.clone()).await?;
            let session = register(&mut client).await?;
            assert_eq!(register(&mut client).await?, session);
            let mut legacy = source::source_service_client::SourceServiceClient::connect(address).await?;
            assert_eq!(legacy.submit_event(source::SubmitEventRequest { event: Some(proto_event(99)) }).await.unwrap_err().code(), tonic::Code::FailedPrecondition);
            if !reopened {
                let input = tokio_stream::iter((1..=3).map(|sequence| request(&session, sequence, sequence as i64)).collect::<Vec<_>>());
                let mut output = client.stream_events(input).await?.into_inner();
                for sequence in 1..=2 {
                    let result = output.message().await?.context("stream receipt")?.result.context("result")?;
                    let proto::admit_response::Result::Receipt(receipt) = result else { anyhow::bail!("{result:?}"); };
                    assert_eq!((receipt.sequence, receipt.position), (sequence, sequence));
                    assert_eq!(receipt.session.as_ref(), Some(&session));
                }
                let result = output.message().await?.context("full-channel response")?.result.context("result")?;
                assert!(matches!(result, proto::admit_response::Result::Error(error) if error.kind == proto::ErrorKind::CapacityExhausted as i32));
                assert!(output.message().await?.is_none(), "error terminates stream without admitting later input");
                assert!(seen.lock().unwrap().is_empty());
                assert_eq!(channel.progress().await?.accepted, 2);
                let result = client.retire_producer(proto::SessionRequest { source_id:"facilities-db".into(), session:Some(session.clone()) }).await?.into_inner().result;
                assert!(matches!(result, Some(proto::retire_response::Result::Error(error)) if error.kind == proto::ErrorKind::PendingObligations as i32));
                return Ok(session);
            }
            assert_eq!(Some(&session), saved.as_ref());
            while channel.progress().await?.processed["consumer"] != 2 { tokio::task::yield_now().await; }
            assert!(matches!(admit(&mut client, request(&session, 2, 20)).await?,
                proto::admit_response::Result::Error(error) if error.kind == proto::ErrorKind::PayloadConflict as i32));
            let original = admit(&mut client, request(&session, 2, 2)).await?;
            let lookup = client.admission_receipt(proto::ReceiptRequest { source_id:"facilities-db".into(), session:Some(session.clone()), sequence:2 }).await?.into_inner().result;
            let proto::admit_response::Result::Receipt(original) = original else { anyhow::bail!("{original:?}"); };
            assert_eq!(lookup, Some(proto::receipt_response::Result::Receipt(original)));
            assert!(matches!(admit(&mut client, request(&session, 3, 3)).await?, proto::admit_response::Result::Receipt(value) if value.position == 3));
            while channel.progress().await?.processed["consumer"] != 3 { tokio::task::yield_now().await; }
            assert_eq!(*seen.lock().unwrap(), [1,2,3]);
            let status = client.producer_status(proto::SessionRequest { source_id:"facilities-db".into(), session:Some(session.clone()) }).await?.into_inner().result;
            assert!(matches!(status, Some(proto::status_response::Result::Status(value)) if value.next_sequence == Some(4) && value.earliest_receipt == Some(2)));
            let expired = client.admission_receipt(proto::ReceiptRequest { source_id:"facilities-db".into(), session:Some(session.clone()), sequence:1 }).await?.into_inner().result;
            assert!(matches!(expired, Some(proto::receipt_response::Result::Error(error)) if error.kind == proto::ErrorKind::ReceiptExpired as i32));
            let retired = client.retire_producer(proto::SessionRequest { source_id:"facilities-db".into(), session:Some(session.clone()) }).await?.into_inner().result;
            assert_eq!(retired, Some(proto::retire_response::Result::Retired(true)));
            assert_ne!(register(&mut client).await?.epoch, session.epoch);
            Ok(session)
        }).await?;
        saved = Some(session);
        drop(channel);
        harness.shutdown().await?;
    }
    Ok(())
}
