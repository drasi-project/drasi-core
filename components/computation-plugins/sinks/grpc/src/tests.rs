// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentId, InputBinding, InputEnvelope, OutputEndpoint, PortId, StreamId,
};
use drasi_computation_plugin_sdk::lifecycle::{
    CancellationToken, LifecycleErrorKind, OperationScope, StartContext, StopMode, UpdateBoundary,
};
use drasi_computation_plugin_sdk::testing::{
    contexts, create_sink, update_request, SinkConformance, SinkProbe,
};
use drasi_computation_plugin_sdk::NativeSink;
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use proto::reaction::{
    self, change_service_server::ChangeService, change_service_server::ChangeServiceServer,
};
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::Notify;
use tonic::{
    metadata::MetadataMap,
    transport::{Certificate, Identity, ServerTlsConfig},
    Code,
};

const ID: &str = "8f0c2d1e-3b4a-4c5d-9e6f-7a8b9c0d1e2f";
const Q: &str = "1b2c3d4e-5f60-4a7b-8c9d-0e1f2a3b4c5d";

// ----- receiver -------------------------------------------------------------

#[derive(Default)]
struct State {
    requests: Mutex<Vec<(MetadataMap, reaction::ProcessChangesRequest)>>,
    /// Successfully acknowledged requests.
    delivered: AtomicUsize,
    /// Fail the next `failures` calls with `code`; `usize::MAX` fails all.
    failure: Mutex<Option<(Code, usize)>>,
    partial: AtomicBool,
    reject: AtomicBool,
    stall: AtomicBool,
    changed: Notify,
}

impl State {
    fn fail(&self, code: Code, times: usize) {
        *self.failure.lock().expect("lock") = Some((code, times));
    }
    fn heal(&self) {
        *self.failure.lock().expect("lock") = None;
    }
    fn calls(&self) -> usize {
        self.requests.lock().expect("lock").len()
    }
    fn header(&self, index: usize, name: &str) -> Option<String> {
        self.requests.lock().expect("lock")[index]
            .0
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }
    fn request(&self, index: usize) -> reaction::ProcessChangesRequest {
        self.requests.lock().expect("lock")[index].1.clone()
    }
}

struct Recorder(Arc<State>);

#[tonic::async_trait]
impl ChangeService for Recorder {
    async fn process_changes(
        &self,
        request: tonic::Request<reaction::ProcessChangesRequest>,
    ) -> std::result::Result<tonic::Response<reaction::ProcessResultsResponse>, tonic::Status> {
        let count = request
            .get_ref()
            .changes
            .as_ref()
            .map_or(0, |batch| batch.changes.len());
        self.0
            .requests
            .lock()
            .expect("lock")
            .push((request.metadata().clone(), request.into_inner()));
        self.0.changed.notify_waiters();
        if self.0.stall.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        {
            let mut failure = self.0.failure.lock().expect("lock");
            if let Some((code, remaining)) = failure.as_mut() {
                let code = *code;
                if *remaining != usize::MAX {
                    *remaining -= 1;
                }
                if *remaining == 0 {
                    *failure = None;
                }
                return Err(tonic::Status::new(code, "injected failure"));
            }
        }
        if self.0.reject.load(Ordering::SeqCst) {
            return Ok(tonic::Response::new(reaction::ProcessResultsResponse {
                success: false,
                message: String::new(),
                error: "rejected by receiver".into(),
                items_processed: 0,
            }));
        }
        let processed = if self.0.partial.load(Ordering::SeqCst) {
            count.saturating_sub(1)
        } else {
            self.0.delivered.fetch_add(1, Ordering::SeqCst);
            count
        };
        self.0.changed.notify_waiters();
        Ok(tonic::Response::new(reaction::ProcessResultsResponse {
            success: true,
            message: String::new(),
            error: String::new(),
            items_processed: processed as u32,
        }))
    }
}

struct Receiver {
    address: SocketAddr,
    state: Arc<State>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Receiver {
    async fn start(address: &str, tls: Option<ServerTlsConfig>) -> Result<Self> {
        Self::with_state(address, tls, Arc::default()).await
    }

    async fn with_state(
        address: &str,
        tls: Option<ServerTlsConfig>,
        state: Arc<State>,
    ) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind(address).await?;
        let address = listener.local_addr()?;
        let mut builder = tonic::transport::Server::builder();
        if let Some(tls) = tls {
            builder = builder.tls_config(tls)?;
        }
        let router = builder.add_service(
            ChangeServiceServer::new(Recorder(state.clone()))
                .accept_compressed(tonic::codec::CompressionEncoding::Gzip),
        );
        let task = tokio::spawn(async move {
            let _ = router
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await;
        });
        Ok(Self {
            address,
            state,
            task,
        })
    }

    fn endpoint(&self) -> String {
        format!("grpc://{}", self.address)
    }
}

// ----- inputs and configuration ----------------------------------------------

fn inputs() -> Vec<InputBinding> {
    vec![InputBinding {
        input_port: PortId::try_new("in").expect("port"),
        output: OutputEndpoint::new(
            ComponentId::try_new(Q).expect("id"),
            PortId::try_new("out").expect("port"),
        ),
        stream: StreamId::try_new("q/out").expect("stream"),
    }]
}

fn room(id: &str, name: &str) -> SourceChange {
    let mut properties = ElementPropertyMap::new();
    properties.insert("name", ElementValue::String(name.into()));
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(Q, id),
                labels: Arc::from([Arc::<str>::from("Room")]),
                effective_from: 1_000,
            },
            properties,
        },
    }
}

fn input_of(sequence: u64, rooms: usize) -> Result<InputEnvelope> {
    let changes: Vec<_> = (0..rooms)
        .map(|n| room(&format!("r{n}"), &format!("room {n}")))
        .collect();
    Ok(InputEnvelope {
        execution_context: Default::default(),
        port: PortId::try_new("in")?,
        envelope: drasi_computation_contracts::GraphChangeCodec::encode_changes(
            &changes,
            StreamId::try_new("q/out")?,
            sequence,
            chrono::DateTime::from_timestamp(1_700_000_000, 0),
        )?,
    })
}

fn input(sequence: u64) -> Result<InputEnvelope> {
    input_of(sequence, 1)
}

fn base(endpoint: &str) -> Value {
    json!({
        "outputs": {"q": {"component": Q, "port": "out"}},
        "endpoint": endpoint,
        "maxRetries": 2,
        "connectTimeoutMs": 2000,
        "timeoutMs": 5000,
    })
}

fn with(mut value: Value, extra: Value) -> Value {
    if let (Some(value), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
        value.extend(extra.clone());
    }
    value
}

fn sink(configuration: Value) -> Result<Box<dyn NativeSink>> {
    create_sink(
        &GrpcSinkFactory,
        &ComponentId::try_new(ID)?,
        configuration,
        &inputs(),
    )
}

async fn started(configuration: Value) -> Result<Box<dyn NativeSink>> {
    let mut sink = sink(configuration)?;
    sink.initialize(&contexts::initialize()).await?;
    sink.start(&mut contexts::start()).await?;
    Ok(sink)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .expect("free port")
}

struct Probe(Arc<State>);

#[async_trait]
impl SinkProbe for Probe {
    fn input(&self, sequence: u64) -> Result<InputEnvelope> {
        input(sequence)
    }
    async fn expect_delivered(&self, _configuration: &Value, count: usize) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let changed = self.0.changed.notified();
                if self.0.delivered.load(Ordering::SeqCst) >= count {
                    return;
                }
                changed.await;
            }
        })
        .await
        .context("delivery not observed")
    }
    async fn set_available(&self, available: bool) -> Result<bool> {
        if available {
            self.0.heal();
        } else {
            self.0.fail(Code::Unavailable, usize::MAX);
        }
        Ok(true)
    }
}

fn conformance() -> SinkConformance<'static> {
    SinkConformance::new(&GrpcSinkFactory, ID, inputs()).expect("conformance")
}

// ----- conformance -------------------------------------------------------------

#[test]
fn metadata_is_valid() -> Result<()> {
    plugin()?;
    let metadata = GrpcSinkFactory.metadata();
    assert!(metadata.capabilities.management && metadata.capabilities.erases_owned_state);
    assert!(GrpcSinkFactory.update_planner().is_some());
    let fields = &metadata.configuration.fields;
    assert!(fields["secretMetadata"].secret && fields["tlsClientPrivateKeyPem"].secret);
    assert!(!fields["metadata"].secret);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn lifecycle_conformance_current_thread() -> Result<()> {
    let receiver = Receiver::start("127.0.0.1:0", None).await?;
    conformance()
        .lifecycle(base(&receiver.endpoint()), &Probe(receiver.state.clone()))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_conformance_multi_thread() -> Result<()> {
    let receiver = Receiver::start("127.0.0.1:0", None).await?;
    let configuration = with(
        base(&receiver.endpoint()),
        json!({"requireConnectedAtStart": true, "compression": "gzip"}),
    );
    conformance()
        .lifecycle(configuration, &Probe(receiver.state.clone()))
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn failed_input_is_retried_without_restart() -> Result<()> {
    let receiver = Receiver::start("127.0.0.1:0", None).await?;
    conformance()
        .retry_after_failure(
            with(base(&receiver.endpoint()), json!({"maxRetries": 1})),
            &Probe(receiver.state.clone()),
        )
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn erase_is_idempotent() -> Result<()> {
    let receiver = Receiver::start("127.0.0.1:0", None).await?;
    conformance().erase(base(&receiver.endpoint())).await
}

#[test]
fn configuration_round_trips() -> Result<()> {
    let server = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    conformance().configuration_round_trip(&[
        json!({}),
        base("grpc://127.0.0.1:50052"),
        with(
            base("grpcs://receiver.example:443"),
            json!({
                "tlsCaPem": server.cert.pem(),
                "tlsClientCertificatePem": server.cert.pem(),
                "tlsClientPrivateKeyPem": server.key_pair.serialize_pem(),
                "tlsDomainName": "receiver.example",
                "metadata": {"x-tenant": "a"},
                "secretMetadata": {"authorization": "Bearer token"},
                "requireConnectedAtStart": true,
                "retryBudgetMs": 5000,
                "keepaliveIntervalMs": 0,
                "compression": "gzip",
                "adaptive": {"minBatchSize": 1, "maxBatchSize": 50, "minWaitMs": 1, "maxWaitMs": 1000, "windowMs": 1000},
                "outputTemplates": {"routes": {"q": {"added": {"template": "{\"n\":{{sequence}}}", "metadata": {"x-op": "{{operation}}"}}}}},
                "outputFormat": "proto",
            }),
        ),
    ])
}

#[test]
fn assessment_matrix() -> Result<()> {
    let current = base("grpc://127.0.0.1:50052");
    let pause = Some(UpdateBoundary::Pause);
    let stop = Some(UpdateBoundary::Stop);
    let case = |extra: Value, boundary| (with(current.clone(), extra), boundary);
    conformance().assess_matrix(
        &current,
        &[
            (current.clone(), pause),
            case(json!({"metadata": {"x-a": "b"}}), pause),
            case(json!({"secretMetadata": {"authorization": "x"}}), pause),
            case(json!({"timeoutMs": 10}), pause),
            case(json!({"maxRetries": 0}), pause),
            case(json!({"retryBudgetMs": 10}), pause),
            case(
                json!({"onPermanentFailure": "skip"}),
                Some(UpdateBoundary::Replace),
            ),
            case(json!({"requireConnectedAtStart": true}), pause),
            case(json!({"batchSize": 10}), pause),
            case(json!({"batchFlushTimeoutMs": 10}), pause),
            case(json!({"maxBatchBytes": 1024}), pause),
            case(
                json!({"outputTemplates": {"defaultTemplate": {"added": {"template": "{}"}}}}),
                pause,
            ),
            case(json!({"outputFormat": "proto"}), pause),
            case(json!({"endpoint": "grpc://127.0.0.1:50053"}), stop),
            case(json!({"connectTimeoutMs": 10}), stop),
            case(json!({"keepaliveIntervalMs": 0}), stop),
            case(json!({"keepaliveTimeoutMs": 10}), stop),
            case(json!({"compression": "gzip"}), stop),
            case(json!({"maxRequestBytes": 1024}), stop),
            case(
                json!({"outputs": {"other": {"component": Q}}}),
                Some(UpdateBoundary::Replace),
            ),
            case(json!({"batchSize": 0}), None),
            case(json!({"metadata": {"x-drasi-port": "x"}}), None),
            case(
                json!({"outputTemplates": {"routes": {"missing": {"added": {"template": "{}"}}}}}),
                None,
            ),
        ],
    )
}

#[test]
fn changed_inputs_replace_the_sink() -> Result<()> {
    let id = ComponentId::try_new(ID)?;
    let configuration = json!({});
    let mut request = update_request(&GrpcSinkFactory, &id, configuration.clone(), configuration);
    request.current.inputs = inputs();
    let mut moved = inputs();
    moved[0].stream = StreamId::try_new("q/other")?;
    request.proposed.inputs = moved;
    let assessment = GrpcSinkFactory
        .update_planner()
        .expect("factory planner")
        .assess(&request)?;
    assert_eq!(
        assessment
            .plan()
            .map(|plan| UpdateBoundary::from(plan.mode)),
        Some(UpdateBoundary::Replace)
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn in_place_and_restart_updates_apply_to_a_running_sink() -> Result<()> {
    let receiver = Receiver::start("127.0.0.1:0", None).await?;
    let probe = Probe(receiver.state.clone());
    let current = base(&receiver.endpoint());
    conformance()
        .apply(
            current.clone(),
            with(current.clone(), json!({"metadata": {"x-tenant": "b"}})),
            &probe,
        )
        .await?;
    let receiver = Receiver::start("127.0.0.1:0", None).await?;
    let probe = Probe(receiver.state.clone());
    let current = base(&receiver.endpoint());
    conformance()
        .apply(
            current.clone(),
            with(current, json!({"connectTimeoutMs": 1000})),
            &probe,
        )
        .await?;
    // A Restart change on a running sink is refused without effect.
    let mut sink = started(base(&receiver.endpoint())).await?;
    let mut request = update_request(
        &GrpcSinkFactory,
        &ComponentId::try_new(ID)?,
        base(&receiver.endpoint()),
        with(base(&receiver.endpoint()), json!({"connectTimeoutMs": 10})),
    );
    request.current.inputs = inputs();
    request.proposed.inputs = inputs();
    let management = sink.management().context("management")?;
    let assessment = management.assess(&request)?;
    let update = drasi_computation_plugin_sdk::ApplyUpdateRequest {
        operation_id: "restart-while-running".into(),
        request,
        plan: assessment.plan().context("plan")?,
        fingerprint: None,
    };
    assert!(management
        .apply(&contexts::apply(UpdateBoundary::Stop), &update)
        .await
        .is_err());
    assert_eq!(sink.configuration()?["connectTimeoutMs"], 2000);
    Ok(())
}

// ----- connection policy ---------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn lazy_start_succeeds_while_the_receiver_is_down_and_delivers_once_it_is_up() -> Result<()> {
    let port = free_port();
    let endpoint = format!("grpc://127.0.0.1:{port}");
    let mut sink = started(with(base(&endpoint), json!({"maxRetries": 0}))).await?;
    assert!(
        sink.handle(input(1)?).await.is_err(),
        "delivery to an absent receiver fails under strict"
    );
    let receiver = Receiver::start(&format!("127.0.0.1:{port}"), None).await?;
    sink.handle(input(1)?).await?;
    assert_eq!(receiver.state.delivered.load(Ordering::SeqCst), 1);
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

fn start_context(deadline: Option<Duration>, token: CancellationToken) -> StartContext {
    StartContext::new(
        OperationScope::new(
            "start",
            "test",
            1,
            deadline.map(|deadline| tokio::time::Instant::now() + deadline),
            token,
        ),
        None,
    )
}

#[tokio::test(flavor = "current_thread")]
async fn required_connection_honours_start_cancellation_and_deadline() -> Result<()> {
    let port = free_port();
    let configuration = with(
        base(&format!("grpc://127.0.0.1:{port}")),
        json!({"requireConnectedAtStart": true, "maxRetries": 100, "retryBudgetMs": 600000}),
    );
    let mut sink = sink(configuration)?;
    sink.initialize(&contexts::initialize()).await?;

    let error = sink
        .start(&mut contexts::cancelled_start())
        .await
        .expect_err("pre-cancelled start");
    assert_eq!(error.kind(), LifecycleErrorKind::Cancelled);

    let token = CancellationToken::new();
    let cancel = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();
    });
    let started_at = std::time::Instant::now();
    let error = sink
        .start(&mut start_context(None, token))
        .await
        .expect_err("cancelled while connecting");
    assert_eq!(error.kind(), LifecycleErrorKind::Cancelled);
    assert!(started_at.elapsed() < Duration::from_secs(3));

    let error = sink
        .start(&mut start_context(
            Some(Duration::from_millis(200)),
            CancellationToken::new(),
        ))
        .await
        .expect_err("deadline while connecting");
    assert!(error.is_retryable());
    assert!(
        sink.handle(input(1)?).await.is_err(),
        "a failed start leaves the sink stopped"
    );

    // The object stays startable.
    let receiver = Receiver::start(&format!("127.0.0.1:{port}"), None).await?;
    sink.start(&mut contexts::start()).await?;
    sink.handle(input(1)?).await?;
    assert_eq!(receiver.state.delivered.load(Ordering::SeqCst), 1);
    sink.stop(&contexts::stop(StopMode::Abort)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn required_connection_fails_start_after_its_retry_budget() -> Result<()> {
    let configuration = with(
        base(&format!("grpc://127.0.0.1:{}", free_port())),
        json!({"requireConnectedAtStart": true, "maxRetries": 1, "connectTimeoutMs": 200}),
    );
    let mut sink = sink(configuration)?;
    sink.initialize(&contexts::initialize()).await?;
    let error = sink
        .start(&mut contexts::start())
        .await
        .expect_err("receiver down");
    assert!(error.is_retryable(), "{error:#}");
    Ok(())
}

// ----- delivery and classification --------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn permanent_codes_are_not_retried() -> Result<()> {
    let receiver = Receiver::start("127.0.0.1:0", None).await?;
    let mut sink = started(with(base(&receiver.endpoint()), json!({"maxRetries": 5}))).await?;
    for code in [
        Code::InvalidArgument,
        Code::Unauthenticated,
        Code::PermissionDenied,
        Code::Unimplemented,
        Code::FailedPrecondition,
    ] {
        let before = receiver.state.calls();
        receiver.state.fail(code, usize::MAX);
        let error = sink.handle(input(1)?).await.expect_err("permanent");
        assert!(format!("{error:#}").contains("not retried"), "{error:#}");
        assert_eq!(receiver.state.calls() - before, 1, "{code:?}");
    }
    receiver.state.heal();
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn retryable_codes_retry_with_a_stable_delivery_id() -> Result<()> {
    let receiver = Receiver::start("127.0.0.1:0", None).await?;
    let mut sink = started(with(base(&receiver.endpoint()), json!({"maxRetries": 3}))).await?;
    receiver.state.fail(Code::Unavailable, 1);
    sink.handle(input(7)?).await?;
    receiver.state.fail(Code::ResourceExhausted, 1);
    sink.handle(input(7)?).await?;
    assert_eq!(receiver.state.calls(), 4);
    let expected = format!("{Q}/out/7.0");
    for index in 0..4 {
        assert_eq!(
            receiver
                .state
                .header(index, "x-drasi-delivery-id")
                .as_deref(),
            Some(expected.as_str()),
            "retries and replays carry the same identity"
        );
        assert_eq!(
            receiver.state.request(index).metadata["x-drasi-delivery-id"],
            expected
        );
    }
    assert_eq!(receiver.state.request(0), receiver.state.request(1));

    // Exhausted retries fail the input.
    receiver.state.fail(Code::Unavailable, usize::MAX);
    let before = receiver.state.calls();
    assert!(sink.handle(input(8)?).await.is_err());
    assert_eq!(receiver.state.calls() - before, 4);
    receiver.state.heal();
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn multi_change_requests_carry_a_range_identity() -> Result<()> {
    let receiver = Receiver::start("127.0.0.1:0", None).await?;
    let mut sink = started(with(base(&receiver.endpoint()), json!({"batchSize": 10}))).await?;
    sink.handle(input_of(3, 3)?).await?;
    assert_eq!(receiver.state.calls(), 1);
    assert_eq!(
        receiver.state.header(0, "x-drasi-delivery-id"),
        Some(format!("{Q}/out/3.0-3.2"))
    );
    assert_eq!(
        receiver
            .state
            .request(0)
            .changes
            .context("changes")?
            .changes
            .len(),
        3
    );
    Ok(())
}

fn permanent(error: &anyhow::Error) -> bool {
    drasi_computation_contracts::delivery_failure_class(error)
        .is_some_and(|class| class.is_permanent())
}

#[tokio::test(flavor = "current_thread")]
async fn incomplete_and_rejected_acknowledgements_fail_without_retry() -> Result<()> {
    let receiver = Receiver::start("127.0.0.1:0", None).await?;
    let mut sink = started(with(base(&receiver.endpoint()), json!({"maxRetries": 3}))).await?;
    receiver.state.partial.store(true, Ordering::SeqCst);
    let error = sink.handle(input(1)?).await.expect_err("partial");
    assert!(format!("{error:#}").contains("incomplete"));
    assert!(permanent(&error), "{error:#}");
    assert_eq!(receiver.state.calls(), 1);
    receiver.state.partial.store(false, Ordering::SeqCst);

    receiver.state.reject.store(true, Ordering::SeqCst);
    let error = sink.handle(input(2)?).await.expect_err("rejected");
    assert!(format!("{error:#}").contains("rejected by receiver"));
    assert!(permanent(&error), "{error:#}");
    assert_eq!(receiver.state.calls(), 2);

    // Under skip the rejection is logged and the input still fails
    // permanently, so the graph counts it as skipped.
    let mut skip = started(with(
        base(&receiver.endpoint()),
        json!({"onPermanentFailure": "skip"}),
    ))
    .await?;
    assert!(permanent(
        &skip.handle(input(3)?).await.expect_err("skipped")
    ));
    assert_eq!(receiver.state.calls(), 3);
    receiver.state.reject.store(false, Ordering::SeqCst);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_stalled_receiver_is_bounded_by_the_call_deadline() -> Result<()> {
    let receiver = Receiver::start("127.0.0.1:0", None).await?;
    receiver.state.stall.store(true, Ordering::SeqCst);
    let mut sink = started(with(
        base(&receiver.endpoint()),
        json!({"timeoutMs": 200, "maxRetries": 1}),
    ))
    .await?;
    let started_at = std::time::Instant::now();
    let error = sink.handle(input(1)?).await.expect_err("stalled");
    assert!(started_at.elapsed() < Duration::from_secs(3));
    assert_eq!(receiver.state.calls(), 2, "a deadline expiry is retried");
    assert!(
        !permanent(&error),
        "exhausted retries stay transient: {error:#}"
    );
    Ok(())
}

// ----- templates and metadata ----------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn templates_render_payload_and_metadata_and_failures_follow_the_policy() -> Result<()> {
    let receiver = Receiver::start("127.0.0.1:0", None).await?;
    let templates = |template: &str| {
        json!({"outputTemplates": {"routes": {"q": {"added": {
            "template": template,
            "metadata": {"x-name": "{{after.name}}"}
        }}}}})
    };
    let mut sink = started(with(
        base(&receiver.endpoint()),
        templates("{\"room\": {{json after.name}}, \"seq\": {{sequence}}}"),
    ))
    .await?;
    sink.handle(input(4)?).await?;
    let request = receiver.state.request(0);
    assert_eq!(request.metadata["x-name"], "room 0");
    assert_eq!(
        receiver.state.header(0, "x-name").as_deref(),
        Some("room 0")
    );
    let change = &request.changes.context("changes")?.changes[0];
    let payload = change.payload.as_ref().context("payload")?;
    assert!(payload.fields.contains_key("room") && payload.fields.contains_key("seq"));
    assert!(change.after.is_some(), "typed images are not rewritten");

    for template in ["{\"missing\": {{after.nope}}}", "not json {{sequence}}"] {
        let before = receiver.state.calls();
        let mut stall = started(with(base(&receiver.endpoint()), templates(template))).await?;
        let error = stall.handle(input(5)?).await.expect_err(template);
        assert!(permanent(&error), "{template}: {error:#}");
        let mut skip = started(with(
            with(base(&receiver.endpoint()), templates(template)),
            json!({"onPermanentFailure": "skip"}),
        ))
        .await?;
        assert!(permanent(
            &skip.handle(input(5)?).await.expect_err(template)
        ));
        assert_eq!(
            receiver.state.calls(),
            before,
            "a template failure sends nothing"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn secret_metadata_is_sent_only_as_headers_and_redacted() -> Result<()> {
    let receiver = Receiver::start("127.0.0.1:0", None).await?;
    let configuration = with(
        base(&receiver.endpoint()),
        json!({
            "metadata": {"X-Tenant": "blue"},
            "secretMetadata": {"Authorization": "Bearer top-secret-value"}
        }),
    );
    let parsed = GrpcSinkConfig::parse(configuration.clone())?;
    assert!(!format!("{parsed:?}").contains("top-secret-value"));
    let mut sink = started(configuration.clone()).await?;
    assert_eq!(
        sink.configuration()?["secretMetadata"],
        configuration["secretMetadata"],
        "configuration() is the persistence hook and keeps secrets"
    );
    sink.handle(input(1)?).await?;
    assert_eq!(
        receiver.state.header(0, "authorization").as_deref(),
        Some("Bearer top-secret-value")
    );
    assert_eq!(
        receiver.state.header(0, "x-tenant").as_deref(),
        Some("blue")
    );
    assert_eq!(
        receiver.state.header(0, "x-drasi-component").as_deref(),
        Some(Q)
    );
    assert_eq!(
        receiver.state.header(0, "x-drasi-port").as_deref(),
        Some("out")
    );
    let request = receiver.state.request(0);
    assert_eq!(request.metadata["x-tenant"], "blue");
    assert!(!request.metadata.contains_key("authorization"));
    Ok(())
}

#[test]
fn reserved_metadata_is_rejected_at_construction() {
    for extra in [
        json!({"metadata": {"x-drasi-delivery-id": "x"}}),
        json!({"metadata": {"x-drasi-component": "x"}}),
        json!({"secretMetadata": {"x-drasi-port": "x"}}),
        json!({"outputTemplates": {"defaultTemplate": {"added": {"template": "", "metadata": {"x-drasi-delivery-id": "{{sequence}}"}}}}}),
    ] {
        let configuration = with(base("grpc://127.0.0.1:1"), extra);
        assert!(sink(configuration.clone()).is_err(), "{configuration}");
    }
}

// ----- TLS -----------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn tls_and_mutual_tls_endpoints() -> Result<()> {
    // A workspace build can enable both rustls providers, which leaves no
    // process default; a loaded plugin library is built with ring only.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let server = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let identity = Identity::from_pem(server.cert.pem(), server.key_pair.serialize_pem());
    let receiver = Receiver::start(
        "127.0.0.1:0",
        Some(ServerTlsConfig::new().identity(identity.clone())),
    )
    .await?;
    let port = receiver.address.port();
    let tls = json!({"tlsCaPem": server.cert.pem(), "tlsDomainName": "localhost", "maxRetries": 0});
    let mut secure = started(with(
        base(&format!("grpcs://127.0.0.1:{port}")),
        tls.clone(),
    ))
    .await?;
    secure.handle(input(1)?).await?;
    assert_eq!(receiver.state.delivered.load(Ordering::SeqCst), 1);

    let mut untrusted = started(with(
        base(&format!("https://127.0.0.1:{port}")),
        json!({"tlsDomainName": "localhost", "maxRetries": 0}),
    ))
    .await?;
    assert!(
        untrusted.handle(input(2)?).await.is_err(),
        "an untrusted certificate is refused"
    );
    let mut plaintext = started(with(
        base(&format!("grpc://127.0.0.1:{port}")),
        json!({"maxRetries": 0}),
    ))
    .await?;
    assert!(plaintext.handle(input(3)?).await.is_err());
    assert_eq!(receiver.state.delivered.load(Ordering::SeqCst), 1);

    // Mutual TLS: the receiver requires a client certificate from its CA.
    let ca_key = rcgen::KeyPair::generate()?;
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key)?;
    let client_key = rcgen::KeyPair::generate()?;
    let mut client_params = rcgen::CertificateParams::new(vec!["drasi-sink".to_owned()])?;
    client_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let client = client_params.signed_by(&client_key, &ca, &ca_key)?;
    let mutual = Receiver::start(
        "127.0.0.1:0",
        Some(
            ServerTlsConfig::new()
                .identity(identity)
                .client_ca_root(Certificate::from_pem(ca.pem())),
        ),
    )
    .await?;
    let endpoint = format!("https://127.0.0.1:{}", mutual.address.port());
    let mut anonymous = started(with(base(&endpoint), tls.clone())).await?;
    assert!(
        anonymous.handle(input(1)?).await.is_err(),
        "mutual TLS requires a client certificate"
    );
    let mut authenticated = started(with(
        with(base(&endpoint), tls),
        json!({
            "tlsClientCertificatePem": client.pem(),
            "tlsClientPrivateKeyPem": client_key.serialize_pem(),
        }),
    ))
    .await?;
    authenticated.handle(input(1)?).await?;
    assert_eq!(mutual.state.delivered.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn batching_policy_follows_in_place_updates() -> Result<()> {
    let receiver = Receiver::start("127.0.0.1:0", None).await?;
    let mut sink = started(base(&receiver.endpoint())).await?;
    assert!(sink.batching()?.is_none());
    let current = base(&receiver.endpoint());
    let proposed = with(current.clone(), json!({"batchSize": 2000}));
    let mut request = update_request(
        &GrpcSinkFactory,
        &ComponentId::try_new(ID)?,
        current,
        proposed,
    );
    request.current.inputs = inputs();
    request.proposed.inputs = inputs();
    sink.pause(&contexts::pause()).await?;
    let management = sink.management().context("management")?;
    let plan = management.assess(&request)?.plan().context("plan")?;
    management
        .apply(
            &contexts::apply(UpdateBoundary::Pause),
            &drasi_computation_plugin_sdk::ApplyUpdateRequest {
                operation_id: "batching".into(),
                request,
                plan,
                fingerprint: None,
            },
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    sink.resume(&contexts::resume()).await?;
    let batching = sink.batching()?.context("batching")?;
    assert_eq!((batching.max_inputs, batching.max_operations), (1024, 2000));
    sink.handle_batch(vec![input(1)?, input(2)?]).await?;
    assert_eq!(
        receiver.state.calls(),
        1,
        "compatible changes share a request"
    );
    Ok(())
}
