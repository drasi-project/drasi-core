// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Requires a separately built native library:
//! cargo build --offline -p drasi-computation-network --features dynamic-plugin
//! cargo test --offline -p drasi-computation-network --test network_loopback

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use drasi_computation_network::{
    proto::{reaction, source},
    GRPC_SINK, GRPC_SOURCE, HTTP_SINK, HTTP_SOURCE,
};
use drasi_core::{
    computation::InMemoryComputationProvider,
    evaluation::{context::QueryPartEvaluationContext, variable_value::VariableValue},
    models::{Element, SourceChange},
};
use drasi_host_sdk::computation::{self, NativeComponentProxy, NativePlugin};
use drasi_lib::computation::v1::*;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{mpsc, Notify},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const DEADLINE: Duration = Duration::from_secs(10);
fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("workspace")
        .to_owned()
}
fn plugin() -> Result<Arc<NativePlugin>> {
    static PLUGIN: OnceLock<std::result::Result<Arc<NativePlugin>, String>> = OnceLock::new();
    PLUGIN.get_or_init(|| {
        let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| workspace().join("target"));
        let target = if target.is_absolute() { target } else { workspace().join(target) };
        let path = std::env::var_os("DRASI_NATIVE_NETWORK_PLUGIN").map(PathBuf::from).unwrap_or_else(||
            target.join("debug").join(format!("{}drasi_computation_network{}", std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX)));
        if !path.is_file() {
            return Err(format!("missing {}; first run CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3 cargo build --offline -p drasi-computation-network --features dynamic-plugin", path.display()));
        }
        computation::load(path).map_err(|error| format!("{error:#}"))
    }).clone().map_err(anyhow::Error::msg)
}
fn component(kind: &str, name: &str, config: Value) -> Result<NativeComponentProxy> {
    let plugin = plugin()?;
    plugin
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == kind)
        .context("native factory")?
        .create_component(ComponentId::try_new(name)?, config)
}
fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}
fn stream(name: &str) -> StreamId {
    StreamId::try_new(name).expect("stream")
}
fn http_event(value: i64) -> Value {
    json!({"operation":"insert","timestamp":1234567890000u64,"element":{
        "type":"node","id":format!("row-{value}"),"labels":["Room"],"properties":{"value":value}
    }})
}
fn proto_event(value: i64) -> source::SourceChange {
    source::SourceChange {
        r#type: source::ChangeType::Insert as i32,
        source_id: "facilities-db".into(),
        timestamp: None,
        change: Some(source::source_change::Change::Element(source::Element {
            element: Some(source::element::Element::Node(source::Node {
                metadata: Some(source::ElementMetadata {
                    reference: Some(source::ElementReference {
                        source_id: "facilities-db".into(),
                        element_id: format!("row-{value}"),
                    }),
                    labels: vec!["Room".into()],
                    effective_from: 1234567890000,
                }),
                properties: Some(drasi_reaction_grpc::helpers::convert_json_to_proto_struct(
                    &json!({"value":value}),
                )),
            })),
        })),
    }
}
fn source_value(envelope: &ChangeEnvelope) -> Result<i64> {
    let changes = GraphChangeCodec::decode_changes(envelope)?;
    let [SourceChange::Insert {
        element: Element::Node {
            metadata,
            properties,
        },
    }] = changes.as_slice()
    else {
        anyhow::bail!("expected one inserted node per graph envelope");
    };
    assert_eq!(metadata.effective_from, 1234567);
    match properties.get("value") {
        Some(drasi_core::models::ElementValue::Integer(value)) => Ok(*value),
        _ => anyhow::bail!("missing integer value"),
    }
}
async fn next(source: &mut NativeComponentProxy) -> Result<ChangeEnvelope> {
    let output = tokio::time::timeout(DEADLINE, source.next())
        .await??
        .context("unexpected EOF")?;
    assert_eq!(output.port.as_str(), "out");
    let progress = GraphProducerProgress::from_envelope(&output.envelope)?
        .context("real producer identity")?;
    assert!(!progress.identity().persistent());
    assert_eq!(progress.sequence(), output.envelope.system().sequence());
    Ok(output.envelope)
}

#[tokio::test(flavor = "current_thread")]
async fn http_source_order_batch_backpressure_cancel_and_awaited_stop() -> Result<()> {
    let port = free_port()?;
    let config = json!({"stream":"facilities-db/out","host":"127.0.0.1","port":port,
        "ingressCapacity":1,"timeoutMs":500,"maxMessageBytes":4096,"maxBatchEvents":4});
    let mut source = component(HTTP_SOURCE, "http-input", config)?;
    let before = source.configuration()?;
    assert!(
        TcpListener::bind(("127.0.0.1", port)).await.is_ok(),
        "factory must not bind"
    );
    source.start().await?;
    assert_eq!(source.configuration()?, before);
    let client = reqwest::Client::builder().timeout(DEADLINE).build()?;
    let base = format!("http://127.0.0.1:{port}");
    assert_eq!(
        client.get(format!("{base}/health")).send().await?.status(),
        StatusCode::OK
    );
    let mut occupied = component(HTTP_SOURCE, "occupied", before.clone())?;
    assert!(
        occupied.start().await.is_err(),
        "bind conflict must reach the host"
    );
    occupied.stop().await?;
    let url = format!("{base}/sources/facilities-db/events");
    let bad = client
        .post(format!("{base}/sources/wrong/events"))
        .json(&http_event(0))
        .send()
        .await?;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    assert_eq!(bad.json::<Value>().await?["success"], false);
    assert!(!client
        .post(&url)
        .json(&json!({"operation":"bad"}))
        .send()
        .await?
        .status()
        .is_success());
    assert_eq!(
        client
            .post(&url)
            .body("x".repeat(5000))
            .send()
            .await?
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let send = client
        .post(format!("{url}/batch"))
        .json(&json!({"events":[http_event(1),http_event(2),http_event(3),http_event(4)]}))
        .send();
    let receive = async {
        let mut values = Vec::new();
        for sequence in 1..=4 {
            let envelope = next(&mut source).await?;
            assert_eq!(envelope.system().sequence(), sequence);
            values.push(source_value(&envelope)?);
        }
        Result::<_>::Ok(values)
    };
    let (response, received) =
        tokio::time::timeout(DEADLINE, async { tokio::join!(send, receive) }).await?;
    assert_eq!(response?.json::<Value>().await?["events_processed"], 4);
    assert_eq!(received?, [1, 2, 3, 4]);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), source.next())
            .await
            .is_err()
    );
    assert_eq!(
        client
            .post(&url)
            .json(&http_event(5))
            .send()
            .await?
            .json::<Value>()
            .await?["success"],
        true
    );
    let waiting = client.post(&url).json(&http_event(6)).send();
    tokio::pin!(waiting);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut waiting)
            .await
            .is_err(),
        "full ingress must backpressure"
    );
    assert_eq!(
        client.get(format!("{base}/health")).send().await?.status(),
        StatusCode::OK
    );
    assert_eq!(source_value(&next(&mut source).await?)?, 5);
    assert_eq!(waiting.await?.json::<Value>().await?["success"], true);
    assert_eq!(source_value(&next(&mut source).await?)?, 6);

    let invalid_batch = client
        .post(format!("{url}/batch"))
        .json(&json!({
            "events":[http_event(7), {"operation":"delete","id":""}]
        }))
        .send()
        .await?;
    assert_eq!(invalid_batch.status(), StatusCode::BAD_REQUEST);
    assert_eq!(invalid_batch.json::<Value>().await?["events_processed"], 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), source.next())
            .await
            .is_err(),
        "prevalidation must reject the entire invalid batch"
    );
    let partial = client
        .post(format!("{url}/batch"))
        .json(&json!({"events":[http_event(7),http_event(8)]}))
        .send()
        .await?;
    assert_eq!(partial.status(), StatusCode::SERVICE_UNAVAILABLE);
    let partial = partial.json::<Value>().await?;
    assert_eq!(partial["success"], false);
    assert_eq!(
        partial["events_processed"], 1,
        "partial admission must report the accepted prefix"
    );
    let admitted = next(&mut source).await?;
    assert_eq!(source_value(&admitted)?, 7);
    assert_eq!(admitted.system().sequence(), 7);
    client
        .post(&url)
        .json(&http_event(8))
        .send()
        .await?
        .error_for_status()?;
    let rejected = client.post(&url).json(&http_event(9)).send().await?;
    assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        rejected.json::<Value>().await?["events_processed"],
        0,
        "timeout cannot claim acceptance"
    );
    let blocked = client.post(&url).json(&http_event(10)).send();
    tokio::pin!(blocked);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut blocked)
            .await
            .is_err()
    );
    let (stop, response) =
        tokio::time::timeout(DEADLINE, async { tokio::join!(source.stop(), blocked) }).await?;
    stop?;
    assert_eq!(response?.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(source.next().await.is_err());
    let rebound = TcpListener::bind(("127.0.0.1", port)).await?;
    drop(rebound);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_source_unary_stream_delta_accounting_and_stop_own_open_streams() -> Result<()> {
    let port = free_port()?;
    let mut source = component(
        GRPC_SOURCE,
        "grpc-input",
        json!({
            "stream":"facilities-db/out","host":"127.0.0.1","port":port,"ingressCapacity":2,"timeoutMs":1000,"maxMessageBytes":1024
        }),
    )?;
    source.start().await?;
    let mut client = source::source_service_client::SourceServiceClient::connect(format!(
        "http://127.0.0.1:{port}"
    ))
    .await?;
    assert_eq!(
        client.health_check(()).await?.into_inner().status,
        source::health_check_response::Status::Healthy as i32
    );
    let error = client
        .request_bootstrap(source::BootstrapRequest {
            query_id: "q".into(),
            node_labels: vec![],
            relation_labels: vec![],
        })
        .await
        .err()
        .context("bootstrap must fail")?;
    assert_eq!(error.code(), tonic::Code::Unimplemented);
    let invalid = client
        .submit_event(source::SubmitEventRequest { event: None })
        .await?
        .into_inner();
    assert!(!invalid.success && !invalid.error.is_empty());
    let mut wrong = proto_event(0);
    wrong.source_id = "wrong".into();
    assert!(
        !client
            .submit_event(source::SubmitEventRequest { event: Some(wrong) })
            .await?
            .into_inner()
            .success
    );
    assert!(
        client
            .submit_event(source::SubmitEventRequest {
                event: Some(proto_event(0))
            })
            .await?
            .into_inner()
            .success
    );
    assert_eq!(source_value(&next(&mut source).await?)?, 0);
    let producer = async {
        let mut response = client
            .stream_events(tokio_stream::iter((1..=150).map(proto_event)))
            .await?
            .into_inner();
        let mut total = 0;
        while let Some(response) = response.message().await? {
            assert!(response.success, "{}", response.error);
            assert_eq!(response.events_processed, 1);
            total += response.events_processed;
        }
        Result::<_>::Ok(total)
    };
    let consumer = async {
        for value in 1..=150 {
            let envelope = next(&mut source).await?;
            assert_eq!(source_value(&envelope)?, value);
            assert_eq!(envelope.system().sequence(), value as u64 + 1);
        }
        Result::<()>::Ok(())
    };
    let (count, drained) =
        tokio::time::timeout(DEADLINE, async { tokio::join!(producer, consumer) }).await?;
    assert_eq!(count?, 150);
    drained?;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), source.next())
            .await
            .is_err()
    );
    let mut invalid = proto_event(152);
    invalid.source_id = "wrong".into();
    let mut failed_stream = client
        .stream_events(tokio_stream::iter([proto_event(151), invalid]))
        .await?
        .into_inner();
    let first = failed_stream
        .message()
        .await?
        .context("accepted stream event")?;
    assert!(first.success);
    assert_eq!(first.events_processed, 1);
    let second = failed_stream
        .message()
        .await?
        .context("failed stream event")?;
    assert!(!second.success && !second.error.is_empty());
    assert_eq!(second.events_processed, 0);
    assert!(
        failed_stream.message().await?.is_none(),
        "failure must not be followed by final success"
    );
    assert_eq!(source_value(&next(&mut source).await?)?, 151);
    for value in [152, 153] {
        assert!(
            client
                .submit_event(source::SubmitEventRequest {
                    event: Some(proto_event(value))
                })
                .await?
                .into_inner()
                .success
        );
    }
    {
        let mut health_client = client.clone();
        let pending = client.submit_event(source::SubmitEventRequest {
            event: Some(proto_event(154)),
        });
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut pending)
                .await
                .is_err(),
            "full gRPC ingress must backpressure unary admission"
        );
        assert_eq!(
            health_client.health_check(()).await?.into_inner().status,
            source::health_check_response::Status::Healthy as i32
        );
        assert_eq!(source_value(&next(&mut source).await?)?, 152);
        assert!(pending.await?.into_inner().success);
    }
    assert_eq!(source_value(&next(&mut source).await?)?, 153);
    assert_eq!(source_value(&next(&mut source).await?)?, 154);
    let mut oversized = proto_event(151);
    oversized.source_id = "x".repeat(2000);
    assert_eq!(
        client
            .submit_event(source::SubmitEventRequest {
                event: Some(oversized)
            })
            .await
            .err()
            .context("size rejection")?
            .code(),
        tonic::Code::OutOfRange
    );
    let (send, receive) = mpsc::channel(1);
    let mut response = client
        .stream_events(tokio_stream::wrappers::ReceiverStream::new(receive))
        .await?
        .into_inner();
    let pending = response.message();
    tokio::pin!(pending);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut pending)
            .await
            .is_err()
    );
    let (stop, message) =
        tokio::time::timeout(DEADLINE, async { tokio::join!(source.stop(), pending) }).await?;
    stop?;
    assert!(message.is_err(), "stopped stream must not report success");
    drop(send);
    drop(client);
    assert!(source.next().await.is_err());
    drop(TcpListener::bind(("127.0.0.1", port)).await?);
    Ok(())
}

fn result_input(sequence: u64) -> Result<InputEnvelope> {
    let after = [("value".into(), VariableValue::Integer(42.into()))].into();
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: QueryChangeCodec::encode_evaluation(
            None,
            &ComponentId::try_new("q")?,
            SystemMetadata::new(stream("q/out"), sequence),
            &[QueryPartEvaluationContext::Adding {
                after,
                row_signature: 11,
            }],
            QueryOutputMetadata {
                query_id: "q".into(),
                source_id: Some("facilities-db".into()),
                timestamp: chrono::DateTime::from_timestamp(1_700_000_000, 123)
                    .context("timestamp")?,
                metadata: HashMap::from([("source".into(), json!("facilities-db"))]),
                profiling: None,
            },
        )?
        .context("output")?,
    })
}

struct ReceiverState {
    http: Mutex<Vec<(HeaderMap, Value)>>,
    grpc: Mutex<
        Vec<(
            tonic::metadata::MetadataMap,
            reaction::ProcessResultsRequest,
        )>,
    >,
    failures: AtomicUsize,
    delay: AtomicUsize,
    entered: Notify,
    cancel: CancellationToken,
}
impl ReceiverState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            http: Mutex::new(Vec::new()),
            grpc: Mutex::new(Vec::new()),
            failures: AtomicUsize::new(0),
            delay: AtomicUsize::new(0),
            entered: Notify::new(),
            cancel: CancellationToken::new(),
        })
    }
    fn fail(&self) -> bool {
        self.failures
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_sub(1)
            })
            .is_ok()
    }
    async fn wait(&self) {
        self.entered.notify_one();
        if self.delay.load(Ordering::Acquire) > 0 {
            tokio::select! {
                _ = self.cancel.cancelled() => {},
                _ = tokio::time::sleep(Duration::from_secs(60)) => {},
            }
        }
    }
}
struct ReceiverServer {
    address: SocketAddr,
    state: Arc<ReceiverState>,
    task: Option<JoinHandle<Result<()>>>,
}
impl Drop for ReceiverServer {
    fn drop(&mut self) {
        self.state.cancel.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
impl ReceiverServer {
    async fn stop(&mut self) -> Result<()> {
        self.state.cancel.cancel();
        if let Some(task) = &mut self.task {
            tokio::time::timeout(DEADLINE, task).await???;
        }
        self.task = None;
        Ok(())
    }
}
async fn receive_http(
    State(state): State<Arc<ReceiverState>>,
    headers: HeaderMap,
    Json(value): Json<Value>,
) -> (StatusCode, Json<Value>) {
    state
        .http
        .lock()
        .expect("HTTP capture")
        .push((headers, value));
    state.wait().await;
    (
        if state.fail() {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::OK
        },
        Json(json!({"ok":true})),
    )
}
async fn http_receiver() -> Result<ReceiverServer> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let state = ReceiverState::new();
    let router = Router::new()
        .route("/reaction", post(receive_http))
        .with_state(state.clone());
    let cancel = state.cancel.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(cancel.cancelled_owned())
            .await?;
        Ok(())
    });
    Ok(ReceiverServer {
        address,
        state,
        task: Some(task),
    })
}
struct ReactionReceiver(Arc<ReceiverState>);
#[tonic::async_trait]
impl reaction::reaction_service_server::ReactionService for ReactionReceiver {
    async fn process_results(
        &self,
        request: tonic::Request<reaction::ProcessResultsRequest>,
    ) -> std::result::Result<tonic::Response<reaction::ProcessResultsResponse>, tonic::Status> {
        let headers = request.metadata().clone();
        self.0
            .grpc
            .lock()
            .expect("gRPC capture")
            .push((headers, request.into_inner()));
        self.0.wait().await;
        let fail = self.0.fail();
        Ok(tonic::Response::new(reaction::ProcessResultsResponse {
            success: !fail,
            message: String::new(),
            error: if fail {
                "deliberate receiver failure".into()
            } else {
                String::new()
            },
            items_processed: if fail { 0 } else { 1 },
        }))
    }
}
async fn grpc_receiver() -> Result<ReceiverServer> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let state = ReceiverState::new();
    let cancel = state.cancel.clone();
    let server = reaction::reaction_service_server::ReactionServiceServer::new(ReactionReceiver(
        state.clone(),
    ));
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(server)
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                cancel.cancelled_owned(),
            )
            .await?;
        Ok(())
    });
    Ok(ReceiverServer {
        address,
        state,
        task: Some(task),
    })
}

#[tokio::test(flavor = "current_thread")]
async fn http_sink_awaits_delivery_retries_exact_body_and_reports_explicit_skip() -> Result<()> {
    let mut receiver = http_receiver().await?;
    let base = json!({"queryId":"q","url":format!("http://{}/reaction", receiver.address),"maxRetries":1,"timeoutMs":1000});
    let mut sink = component(HTTP_SINK, "http-output", base.clone())?;
    assert!(sink.handle(result_input(1)?).await.is_err());
    sink.start().await?;
    receiver.state.failures.store(1, Ordering::Release);
    sink.handle(result_input(1)?).await?;
    {
        let delivered = receiver.state.http.lock().expect("capture");
        assert_eq!(delivered.len(), 2);
        assert_eq!(
            delivered[0].1, delivered[1].1,
            "retry preserves logical delivery"
        );
        assert_eq!(delivered[0].0["x-query-sequence"], "q");
        assert_eq!(delivered[0].0["content-type"], "application/json");
        assert_eq!(
            delivered[0].1,
            json!({"operation":"ADD","queryId":"q","sequenceId":1,
            "timestamp":"2023-11-14T22:13:20.000000123+00:00","after":{"value":42},"metadata":{"source":"facilities-db"}})
        );
    }
    receiver.state.failures.store(10, Ordering::Release);
    assert!(
        sink.handle(result_input(2)?).await.is_err(),
        "strict retry exhaustion fails"
    );
    sink.stop().await?;
    let mut config = base;
    config["failurePolicy"] = json!("skip");
    config["maxRetries"] = json!(0);
    let mut skip = component(HTTP_SINK, "http-skip", config)?;
    skip.start().await?;
    skip.handle(result_input(3)?).await?;
    skip.stop().await?;
    receiver.state.failures.store(0, Ordering::Release);
    receiver.state.delay.store(1, Ordering::Release);
    sink.start().await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), sink.handle(result_input(4)?))
            .await
            .is_err(),
        "pending delivery cannot be early Handled success"
    );
    sink.stop().await?;
    assert!(sink.handle(result_input(5)?).await.is_err());
    receiver.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_sink_delivers_item_metadata_headers_retries_and_never_early_success() -> Result<()> {
    let mut receiver = grpc_receiver().await?;
    let config = json!({"queryId":"q","endpoint":format!("grpc://{}", receiver.address),
        "maxRetries":1,"timeoutMs":1000,"connectionRetryAttempts":1,"initialConnectionTimeoutMs":1000,
        "metadata":{"x-custom":"custom","x-query-sequence":"q"}});
    let mut sink = component(GRPC_SINK, "grpc-output", config)?;
    sink.start().await?;
    receiver.state.failures.store(1, Ordering::Release);
    sink.handle(result_input(1)?).await?;
    {
        let delivered = receiver.state.grpc.lock().expect("capture");
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered[0].1, delivered[1].1);
        let (headers, request) = &delivered[0];
        assert_eq!(
            headers
                .get("x-query-sequence")
                .context("query header")?
                .to_str()?,
            "q"
        );
        assert_eq!(
            headers.get("x-custom").context("custom header")?.to_str()?,
            "custom"
        );
        assert_eq!(request.metadata["x-custom"], "custom");
        let output = request.results.as_ref().context("result")?;
        assert_eq!(output.query_id, "q");
        assert_eq!(output.results.len(), 1);
        let item = &output.results[0];
        assert_eq!(item.item_type, reaction::QueryResultItemType::Add as i32);
        assert_eq!(item.sequence, 1);
        assert_eq!(item.row_signature, 11);
        assert_eq!(item.timestamp, output.timestamp);
        assert!(
            item.metadata.is_some()
                && item.payload.is_some()
                && item.after.is_some()
                && item.before.is_none()
        );
    }
    receiver.state.failures.store(10, Ordering::Release);
    assert!(sink.handle(result_input(2)?).await.is_err());
    receiver.state.failures.store(0, Ordering::Release);
    receiver.state.delay.store(1, Ordering::Release);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), sink.handle(result_input(3)?))
            .await
            .is_err()
    );
    sink.stop().await?;
    assert!(sink.handle(result_input(4)?).await.is_err());
    receiver.stop().await?;
    let mut failed = component(
        GRPC_SINK,
        "no-receiver",
        json!({
            "queryId":"q","endpoint":format!("grpc://127.0.0.1:{}", free_port()?),
            "connectionRetryAttempts":1,"initialConnectionTimeoutMs":50,"timeoutMs":50
        }),
    )?;
    assert!(
        failed.start().await.is_err(),
        "connection failure must not be lazy startup success"
    );
    failed.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn both_network_paths_use_one_host_query_evaluation_per_input() -> Result<()> {
    for grpc in [false, true] {
        let mut receiver = if grpc {
            grpc_receiver().await?
        } else {
            http_receiver().await?
        };
        let port = free_port()?;
        let plugin = plugin()?;
        let source_factory = plugin
            .factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref()
                    == if grpc { GRPC_SOURCE } else { HTTP_SOURCE }
            })
            .context("source factory")?
            .clone();
        let sink_factory = plugin
            .factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref()
                    == if grpc { GRPC_SINK } else { HTTP_SINK }
            })
            .context("sink factory")?
            .clone();
        let sink_config = if grpc {
            json!({"queryId":"q","endpoint":format!("grpc://{}", receiver.address),"maxRetries":0})
        } else {
            json!({"queryId":"q","url":format!("http://{}/reaction",receiver.address),"maxRetries":0})
        };
        let query = ContinuousQueryDefinition {
            graph_id: "network-evaluator".into(),
            id: ComponentId::try_new("q")?,
            query: "MATCH (r:Room) RETURN r.value AS value".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: stream("q/out"),
            outbox_capacity: std::num::NonZeroUsize::new(32).unwrap(),
        };
        let indexes = ResourceId::try_new("memory-indexes")?;
        let spec = ComponentSpecification {
            descriptor: query.descriptor(),
            role: ComponentRole::Query,
            completion: None,
            implementation: ImplementationIdentity::try_new("drasi/continuous-query", "1")?,
            configuration_version: 1,
            configuration: [
                (
                    "query".into(),
                    ConfigurationValue::Literal(query.query.clone().into()),
                ),
                ("stream".into(), ConfigurationValue::Literal("q/out".into())),
                (
                    "runtime_compatibility".into(),
                    ConfigurationValue::Literal(true.into()),
                ),
            ]
            .into(),
            dependencies: [("indexes".into(), vec![indexes.clone()])].into(),
        };
        let endpoint = |id: &str, port: &str| -> Result<Endpoint> {
            Ok(Endpoint::new(
                ComponentId::try_new(id)?,
                PortId::try_new(port)?,
            ))
        };
        let mut graph = ComputationGraph::builder("network-evaluator")
            .declare_resource(ResourceSpecification {
                id: indexes.clone(),
                role: ResourceRole::IndexBackend,
                ownership: ResourceOwnership::Borrowed,
                binding: "memory".into(),
            })?
            .provide_resource(
                indexes,
                ResourceHandle::new(
                    ResourceRole::IndexBackend,
                    Arc::new(QueryIndexProviderResource(Arc::new(
                        InMemoryComputationProvider,
                    ))),
                ),
            )?
            .component(
                source_factory.specification(
                    ComponentId::try_new("facilities-db")?,
                    json!({
                        "stream":"facilities-db","host":"127.0.0.1","port":port,"timeoutMs":1000
                    }),
                )?,
                source_factory,
            )
            .component(spec, Arc::new(ContinuousQueryFactory::default()))
            .component(
                sink_factory.specification(ComponentId::try_new("sink")?, sink_config)?,
                sink_factory,
            )
            .connect(
                EdgeDefinition::new(endpoint("facilities-db", "out")?, endpoint("q", "in")?),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                EdgeDefinition::new(endpoint("q", "out")?, endpoint("sink", "in")?),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .bind_stream(endpoint("facilities-db", "out")?, stream("facilities-db"))
            .bind_stream(endpoint("q", "out")?, stream("q/out"))
            .build()?;
        let run = graph.start()?;
        let control = run.control();
        let drive = async {
            let result = async {
                assert_eq!(
                    control.startup_report().await?.summary,
                    OperationSummary::Completed
                );
                let http = reqwest::Client::new();
                let mut grpc_client = if grpc {
                    Some(
                        source::source_service_client::SourceServiceClient::connect(format!(
                            "http://127.0.0.1:{port}"
                        ))
                        .await?,
                    )
                } else {
                    None
                };
                for value in 1..=10 {
                    if let Some(client) = &mut grpc_client {
                        assert!(
                            client
                                .submit_event(source::SubmitEventRequest {
                                    event: Some(proto_event(value))
                                })
                                .await?
                                .into_inner()
                                .success
                        );
                    } else {
                        http.post(format!(
                            "http://127.0.0.1:{port}/sources/facilities-db/events"
                        ))
                        .json(&http_event(value))
                        .send()
                        .await?
                        .error_for_status()?;
                    }
                }
                loop {
                    let arrived = receiver.state.entered.notified();
                    let count = if grpc {
                        receiver.state.grpc.lock().unwrap().len()
                    } else {
                        receiver.state.http.lock().unwrap().len()
                    };
                    if count == 10 {
                        break;
                    }
                    arrived.await;
                }
                Result::<()>::Ok(())
            }
            .await;
            control.cancel();
            result
        };
        let (run, driven) =
            tokio::time::timeout(DEADLINE, async { tokio::join!(run, drive) }).await?;
        driven?;
        assert!(matches!(run, Err(GraphError::Cancelled)));
        let expected_metadata =
            json!({"source_id":"facilities-db","processed_by":"drasi-core","result_count":1});
        if grpc {
            let values = receiver.state.grpc.lock().unwrap();
            assert_eq!(values.len(), 10);
            for (index, (_, request)) in values.iter().enumerate() {
                let item = &request.results.as_ref().context("result")?.results[0];
                assert_eq!(item.sequence, index as u64 + 1);
                assert_eq!(
                    item.metadata,
                    Some(drasi_reaction_grpc::helpers::convert_json_to_proto_struct(
                        &expected_metadata
                    ))
                );
                assert_eq!(
                    item.after,
                    Some(drasi_reaction_grpc::helpers::convert_json_to_proto_struct(
                        &json!({"value":index+1})
                    ))
                );
            }
        } else {
            let values = receiver.state.http.lock().unwrap();
            assert_eq!(values.len(), 10);
            for (index, (_, item)) in values.iter().enumerate() {
                assert_eq!(item["sequenceId"], index as u64 + 1);
                assert_eq!(item["after"], json!({"value":index+1}));
                assert_eq!(item["metadata"], expected_metadata);
            }
        }
        receiver.stop().await?;
    }
    Ok(())
}
