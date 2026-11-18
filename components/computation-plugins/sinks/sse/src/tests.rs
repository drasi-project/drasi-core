// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::component::{test_support, Frame, PathEntry, Running, SseSink, SubscriberSlot};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, InputBinding, InputEnvelope, OutputEndpoint, PortId,
    SinkCompletion, SinkOutputs, StreamId,
};
use drasi_computation_plugin_sdk::lifecycle::{
    ApplyContext, CancellationToken, DisposeContext, InitializeContext, LifecycleErrorKind,
    LifecycleResult, OperationScope, PauseContext, ResumeContext, StartContext, StopContext,
    StopMode, UpdateBoundary,
};
use drasi_computation_plugin_sdk::testing::{
    contexts, create_sink, update_request, SinkConformance, SinkProbe,
};
use drasi_computation_plugin_sdk::{
    Component, ControlSender, CreateRequest, CreatedComponent, Factory, FactoryMetadata,
    NativeComponent, NativeManagement, NativeSink,
};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc, time::Duration};
use tokio::time::Instant;

const ID: &str = "268e23f4-4a61-59ee-8d8f-0ab7a4b9f5c1";
const OUTPUT_COMPONENT_ID: &str = "76b8e87b-4275-567c-a37f-d48582112ba1";
const TOKEN: &str = "0123456789abcdef-token";
const WAIT: Duration = Duration::from_secs(5);

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .expect("free port")
}

fn q_outputs() -> Value {
    json!({"q": {"component": OUTPUT_COMPONENT_ID, "port": "out"}})
}

fn inputs() -> Vec<InputBinding> {
    vec![InputBinding {
        input_port: PortId::try_new("in").expect("port"),
        output: OutputEndpoint::new(
            ComponentId::try_new(OUTPUT_COMPONENT_ID).expect("id"),
            PortId::try_new("out").expect("port"),
        ),
        stream: StreamId::try_new("q/out").expect("stream"),
    }]
}

fn base(port: u16) -> Value {
    json!({"outputs": q_outputs(), "port": port})
}

fn with(mut value: Value, extra: Value) -> Value {
    if let (Some(value), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
        value.extend(extra.clone());
    }
    value
}

fn input(sequence: u64) -> Result<InputEnvelope> {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(OUTPUT_COMPONENT_ID, &format!("e{sequence}")),
            labels: Arc::from([Arc::from("Item")]),
            effective_from: 1,
        },
        properties: ElementPropertyMap::new(),
    };
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: drasi_computation_contracts::GraphChangeCodec::encode_change(
            SourceChange::Insert { element },
            StreamId::try_new("q/out")?,
            sequence,
            None,
        )?,
    })
}

fn component_id() -> ComponentId {
    ComponentId::try_new(ID).expect("id")
}

async fn started(configuration: Value) -> Result<Box<dyn NativeSink>> {
    let mut sink = create_sink(&SseFactory, &component_id(), configuration, &inputs())?;
    sink.initialize(&contexts::initialize()).await?;
    sink.start(&mut contexts::start()).await?;
    Ok(sink)
}

fn stop_with_deadline(mode: StopMode, deadline: Instant) -> StopContext {
    StopContext::new(
        OperationScope::new("stop", "test", 1, Some(deadline), CancellationToken::new()),
        mode,
    )
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(WAIT)
        .build()
        .expect("client")
}

fn url(port: u16, path: &str) -> String {
    format!("http://127.0.0.1:{port}{path}")
}

/// One parsed SSE frame.
#[derive(Debug, Default)]
struct SseFrame {
    data: Option<String>,
    id: Option<String>,
    comments: Vec<String>,
}

impl SseFrame {
    fn json(&self) -> Option<Value> {
        serde_json::from_str(self.data.as_deref()?).ok()
    }
    fn is_heartbeat(&self) -> bool {
        self.json()
            .is_some_and(|value| value["type"] == "heartbeat")
    }
}

struct EventStream {
    response: reqwest::Response,
    pending: String,
}

impl EventStream {
    fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            pending: String::new(),
        }
    }

    /// The next frame, or `None` when the stream ends.
    async fn frame(&mut self) -> Result<Option<SseFrame>> {
        loop {
            if let Some(end) = self.pending.find("\n\n") {
                let raw: String = self.pending.drain(..end + 2).collect();
                let mut frame = SseFrame::default();
                let mut data = Vec::new();
                for line in raw.lines() {
                    if let Some(value) = line.strip_prefix("data:") {
                        data.push(value.trim_start().to_owned());
                    } else if let Some(value) = line.strip_prefix("id:") {
                        frame.id = Some(value.trim_start().to_owned());
                    } else if let Some(value) = line.strip_prefix(':') {
                        frame.comments.push(value.trim_start().to_owned());
                    }
                }
                if !data.is_empty() {
                    frame.data = Some(data.join("\n"));
                }
                return Ok(Some(frame));
            }
            match self.response.chunk().await? {
                Some(bytes) => self.pending.push_str(std::str::from_utf8(&bytes)?),
                None => return Ok(None),
            }
        }
    }

    /// The next non-heartbeat data frame.
    async fn data(&mut self) -> Result<SseFrame> {
        tokio::time::timeout(WAIT, async {
            loop {
                let frame = self.frame().await?.context("SSE stream ended")?;
                if frame.data.is_some() && !frame.is_heartbeat() {
                    return Ok(frame);
                }
            }
        })
        .await?
    }
}

async fn open(request: reqwest::RequestBuilder) -> Result<EventStream> {
    let response = request.send().await?.error_for_status()?;
    Ok(EventStream::new(response))
}

// ---------------------------------------------------------------- the probe

/// Observes delivery through real SSE clients. Volatile broadcasts reach only
/// subscribers connected when `handle` runs, so [`Observed`] asks the probe to
/// subscribe before every delivery. Sequences are de-duplicated across the
/// probe's connections, so overlapping subscriptions never double count.
struct SseProbe {
    client: reqwest::Client,
    seen: Arc<std::sync::Mutex<BTreeSet<u64>>>,
    readers: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl SseProbe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            client: client(),
            seen: Arc::default(),
            readers: std::sync::Mutex::default(),
        })
    }

    /// Opens one subscription at the sink's configured endpoint; false when
    /// the sink refuses or is not listening.
    async fn subscribe(&self, configuration: &Value) -> bool {
        let address = format!(
            "http://{}:{}{}",
            configuration["host"].as_str().unwrap_or("127.0.0.1"),
            configuration["port"],
            configuration["ssePath"].as_str().unwrap_or("/events"),
        );
        let mut request = self.client.get(address);
        if let Some(token) = configuration["authToken"].as_str() {
            request = request.bearer_auth(token);
        }
        let Ok(response) = request.send().await else {
            return false;
        };
        if !response.status().is_success() {
            return false;
        }
        let seen = self.seen.clone();
        let reader = tokio::spawn(async move {
            let mut stream = EventStream::new(response);
            while let Ok(Some(frame)) = stream.frame().await {
                if let Some(sequence) = frame.json().and_then(|value| value["sequence"].as_u64()) {
                    seen.lock().expect("seen").insert(sequence);
                }
            }
        });
        self.readers.lock().expect("readers").push(reader);
        true
    }
}

impl Drop for SseProbe {
    fn drop(&mut self) {
        if let Ok(readers) = self.readers.get_mut() {
            for reader in readers.drain(..) {
                reader.abort();
            }
        }
    }
}

#[async_trait]
impl SinkProbe for SseProbe {
    fn input(&self, sequence: u64) -> Result<InputEnvelope> {
        input(sequence)
    }
    async fn expect_delivered(&self, _configuration: &Value, count: usize) -> Result<()> {
        tokio::time::timeout(WAIT, async {
            while self.seen.lock().expect("seen").len() < count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .with_context(|| {
            format!(
                "SSE subscribers observed {:?}, expected {count} deliveries",
                self.seen
                    .lock()
                    .map(|seen| seen.clone())
                    .unwrap_or_default()
            )
        })
    }
}

/// The real factory, with every created sink wrapped so the probe subscribes
/// before each `handle`. Everything else is delegated unchanged.
struct Observed {
    probe: Arc<SseProbe>,
}

impl Factory for Observed {
    fn describe(
        &self,
        _request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        Ok(drasi_computation_plugin_sdk::ComponentDescription::undeclared())
    }
    fn metadata(&self) -> FactoryMetadata {
        SseFactory.metadata()
    }
    fn configuration_details(&self) -> Option<drasi_computation_plugin_sdk::ConfigDetails> {
        SseFactory.configuration_details()
    }
    fn supports_update_planning(&self) -> bool {
        SseFactory.supports_update_planning()
    }
    fn assess(
        &self,
        request: &drasi_computation_plugin_sdk::UpdateRequest,
    ) -> Result<drasi_computation_plugin_sdk::lifecycle::Assessment> {
        SseFactory.assess(request)
    }
    fn create(&self, request: &CreateRequest, control: ControlSender) -> Result<CreatedComponent> {
        match SseFactory.create(request, control)?.component {
            Component::Sink(inner) => Ok(Component::Sink(Box::new(ObservedSink {
                inner,
                probe: self.probe.clone(),
            }))
            .into()),
            _ => anyhow::bail!("SSE factory did not create a sink"),
        }
    }
}

struct ObservedSink {
    inner: Box<dyn NativeSink>,
    probe: Arc<SseProbe>,
}

#[async_trait]
impl NativeComponent for ObservedSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.inner.descriptor()
    }
    fn configuration(&self) -> Result<Value> {
        self.inner.configuration()
    }
    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        self.inner.management()
    }
    async fn initialize(&mut self, ctx: &InitializeContext) -> LifecycleResult {
        self.inner.initialize(ctx).await
    }
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        self.inner.start(ctx).await
    }
    async fn pause(&mut self, ctx: &PauseContext) -> LifecycleResult {
        self.inner.pause(ctx).await
    }
    async fn resume(&mut self, ctx: &ResumeContext) -> LifecycleResult {
        self.inner.resume(ctx).await
    }
    async fn stop(&mut self, ctx: &StopContext) -> LifecycleResult {
        self.inner.stop(ctx).await
    }
    async fn dispose(&mut self, ctx: &DisposeContext) -> LifecycleResult {
        self.inner.dispose(ctx).await
    }
}

#[async_trait]
impl NativeSink for ObservedSink {
    fn completion(&self) -> SinkCompletion {
        self.inner.completion()
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        if let Ok(configuration) = self.inner.configuration() {
            self.probe.subscribe(&configuration).await;
        }
        self.inner.handle(input).await
    }
}

fn conformance(factory: &dyn Factory) -> SinkConformance<'_> {
    SinkConformance::new(factory, ID, inputs()).expect("conformance")
}

// ---------------------------------------------------------- conformance kit

#[test]
fn metadata_is_valid() -> Result<()> {
    plugin()?;
    let capabilities = SseFactory.metadata().capabilities;
    assert!(capabilities.management && capabilities.erases_owned_state);
    assert!(SseFactory.supports_update_planning());
    let details = SseFactory.configuration_details().context("details")?;
    details.validate_schema(&SseFactory.metadata().configuration)?;
    let schema = &SseFactory.metadata().configuration;
    for secret in ["authToken", "authTokenSecondary", "tlsPrivateKeyPem"] {
        assert!(schema.fields[secret].secret, "{secret} must be secret");
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn lifecycle_conformance_current_thread() -> Result<()> {
    let factory = Observed {
        probe: SseProbe::new(),
    };
    conformance(&factory)
        .lifecycle(base(free_port()), factory.probe.as_ref())
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_conformance_multi_thread() -> Result<()> {
    let factory = Observed {
        probe: SseProbe::new(),
    };
    conformance(&factory)
        .lifecycle(
            with(base(free_port()), json!({"authToken": TOKEN})),
            factory.probe.as_ref(),
        )
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn erase_is_an_idempotent_no_op() -> Result<()> {
    conformance(&SseFactory).erase(base(free_port())).await
}

#[test]
fn configuration_round_trips() -> Result<()> {
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    conformance(&SseFactory).configuration_round_trip(&[
        base(9100),
        json!({}),
        with(
            base(9101),
            json!({
                "authToken": TOKEN, "authTokenSecondary": "fedcba9876543210-token",
                "allowQueryToken": false, "corsAllowedOrigins": ["https://app.example"],
                "rateLimitPerSecond": 10, "eventIds": true, "maxSubscribers": 5,
                "keepAliveIntervalMs": 1000, "headerReadTimeoutMs": 500,
            }),
        ),
        with(
            base(9102),
            json!({"routes": {"q": {"added": {"template": "{{json after}}", "path": "items"}}}}),
        ),
        with(
            base(9103),
            json!({
                "tlsCertificatePem": certificate.cert.pem(),
                "tlsPrivateKeyPem": certificate.key_pair.serialize_pem(),
            }),
        ),
        with(
            base(9104),
            json!({"host": "0.0.0.0", "allowUnauthenticated": true}),
        ),
    ])
}

#[test]
fn assessment_matrix() -> Result<()> {
    use UpdateBoundary::*;
    let current = base(9000);
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let change = |extra: Value| with(current.clone(), extra);
    conformance(&SseFactory).assess_matrix(
        &current,
        &[
            (current.clone(), Some(Pause)),
            (
                change(json!({"routes": {"q": {"added": {"template": "x"}}}})),
                Some(Pause),
            ),
            (
                change(json!({"defaultTemplate": {"added": {"template": "x"}}})),
                Some(Pause),
            ),
            (change(json!({"heartbeatIntervalMs": 5})), Some(Pause)),
            (change(json!({"keepAliveIntervalMs": 5})), Some(Pause)),
            (change(json!({"eventIds": true})), Some(Pause)),
            (change(json!({"authToken": TOKEN})), Some(Pause)),
            (change(json!({"allowQueryToken": false})), Some(Pause)),
            (change(json!({"corsAllowedOrigins": ["*"]})), Some(Pause)),
            (change(json!({"rateLimitPerSecond": 3})), Some(Pause)),
            (change(json!({"maxSubscribers": 3})), Some(Pause)),
            (change(json!({"port": 9001})), Some(Stop)),
            (change(json!({"host": "::1"})), Some(Stop)),
            (change(json!({"ssePath": "/stream"})), Some(Stop)),
            (change(json!({"broadcastCapacity": 3})), Some(Stop)),
            (change(json!({"maxPaths": 3})), Some(Stop)),
            (change(json!({"maxConnections": 3})), Some(Stop)),
            (change(json!({"headerReadTimeoutMs": 3})), Some(Stop)),
            (
                change(json!({
                    "tlsCertificatePem": certificate.cert.pem(),
                    "tlsPrivateKeyPem": certificate.key_pair.serialize_pem(),
                })),
                Some(Stop),
            ),
            (
                change(
                    json!({"outputs": {"r": {"component": OUTPUT_COMPONENT_ID, "port": "out"}}}),
                ),
                Some(Replace),
            ),
            (change(json!({"heartbeatIntervalMs": 0})), None),
            (change(json!({"host": "0.0.0.0"})), None),
            (change(json!({"routes": {"missing": {}}})), None),
            (
                change(json!({"corsAllowedOrigins": ["*", "https://a.example"]})),
                None,
            ),
        ],
    )?;
    // A change to the incoming edges replaces the sink.
    let mut request = update_request(&SseFactory, &component_id(), current.clone(), current);
    request.current.inputs = inputs();
    let mut moved = inputs();
    moved[0].stream = StreamId::try_new("q/other")?;
    request.proposed.inputs = moved;
    assert_eq!(
        SseFactory
            .assess(&request)?
            .plan()
            .map(|plan| UpdateBoundary::from(plan.mode)),
        Some(Replace)
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn in_place_and_restart_updates_apply_to_a_running_sink() -> Result<()> {
    let factory = Observed {
        probe: SseProbe::new(),
    };
    let port = free_port();
    // In place: a new token, CORS and heartbeat take effect without a restart.
    conformance(&factory)
        .apply(
            base(port),
            with(
                base(port),
                json!({"authToken": TOKEN, "corsAllowedOrigins": ["*"], "heartbeatIntervalMs": 1000}),
            ),
            factory.probe.as_ref(),
        )
        .await?;
    let factory = Observed {
        probe: SseProbe::new(),
    };
    let port = free_port();
    conformance(&factory)
        .apply(
            base(port),
            with(base(port), json!({"ssePath": "/stream"})),
            factory.probe.as_ref(),
        )
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn apply_refuses_stale_plans_and_unsettled_boundaries() -> Result<()> {
    let port = free_port();
    let mut sink = started(base(port)).await?;
    let proposed = with(base(port), json!({"eventIds": true}));
    let mut request = update_request(&SseFactory, &component_id(), base(port), proposed);
    request.current.inputs = inputs();
    request.proposed.inputs = inputs();
    let management = sink.management().context("management")?;
    let assessment = management.assess(&request)?;
    let update = drasi_computation_plugin_sdk::ApplyUpdateRequest {
        operation_id: "test".into(),
        request: request.clone(),
        plan: assessment.plan().context("plan")?,
        fingerprint: None,
    };
    // Not paused: an in-place update is refused and nothing changes.
    assert!(management
        .apply(
            &ApplyContext::new(contexts::scope("apply"), UpdateBoundary::Pause),
            &update
        )
        .await
        .is_err());
    assert_eq!(sink.configuration()?["eventIds"], false);
    // A forged Stop plan for a Pause change is refused.
    let mut forged = update.clone();
    forged.plan = drasi_computation_plugin_sdk::lifecycle::Assessment::supported(
        UpdateBoundary::Stop,
        drasi_computation_contracts::ComponentUpdateData::Preserve,
    )
    .plan()
    .context("plan")?;
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    assert!(sink
        .management()
        .context("management")?
        .apply(&contexts::apply(UpdateBoundary::Stop), &forged)
        .await
        .is_err());
    Ok(())
}

// ------------------------------------------------------------- pause / stop

#[tokio::test(flavor = "current_thread")]
async fn pause_refuses_new_subscribers_and_snapshots_and_resume_reopens() -> Result<()> {
    let port = free_port();
    let mut sink = started(with(
        base(port),
        json!({"heartbeatIntervalMs": 20, "keepAliveIntervalMs": 20}),
    ))
    .await?;
    let client = client();
    let mut existing = open(client.get(url(port, "/events"))).await?;
    sink.pause(&contexts::pause()).await?;
    let paused_at = Instant::now();
    for path in ["/events", "/snapshot/q"] {
        let response = client.get(url(port, path)).send().await?;
        assert_eq!(
            response.status(),
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "{path}"
        );
        assert_eq!(response.headers()["retry-after"], "1");
    }
    assert!(
        sink.handle(input(1)?).await.is_err(),
        "paused sink refuses input"
    );
    // The existing subscriber stays connected: heartbeats stop, keep-alive
    // comments continue.
    let mut late_heartbeat = false;
    let mut late_comment = false;
    while paused_at.elapsed() < Duration::from_millis(400) {
        let frame = tokio::time::timeout(WAIT, existing.frame())
            .await??
            .context("existing subscriber stays connected while paused")?;
        let late = paused_at.elapsed() > Duration::from_millis(100);
        late_heartbeat |= late && frame.is_heartbeat();
        late_comment |= late && frame.comments.iter().any(|c| c == "keep-alive");
    }
    assert!(!late_heartbeat, "heartbeats are suspended while paused");
    assert!(late_comment, "keep-alive comments continue while paused");

    sink.resume(&contexts::resume()).await?;
    let response = client.get(url(port, "/snapshot/q")).send().await?;
    assert_eq!(response.status(), reqwest::StatusCode::NOT_IMPLEMENTED);
    let mut fresh = open(client.get(url(port, "/events"))).await?;
    sink.handle(input(2)?).await?;
    for stream in [&mut existing, &mut fresh] {
        assert_eq!(stream.data().await?.json().context("json")?["sequence"], 2);
    }
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_pause_reopens() -> Result<()> {
    let port = free_port();
    let mut sink = started(base(port)).await?;
    let scope = contexts::scope("pause");
    scope.cancellation().cancel();
    // Pause with no in-flight reads completes; the biased cancellation wins.
    let error = sink
        .pause(&PauseContext::new(scope))
        .await
        .expect_err("cancelled pause");
    assert_eq!(error.kind(), LifecycleErrorKind::Cancelled);
    assert!(client()
        .get(url(port, "/events"))
        .send()
        .await?
        .status()
        .is_success());
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn short_stop_deadline_retains_a_draining_server_and_a_later_stop_succeeds() -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let port = free_port();
    let large = "x".repeat(64 * 1024);
    let mut sink = started(with(
        base(port),
        json!({"maxSubscribers": 1, "routes": {"q": {"added": {"template": large}}}}),
    ))
    .await?;
    // A subscriber that never reads: once its socket buffers fill, the
    // listener cannot finish draining the stream.
    let mut stalled = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    stalled
        .write_all(b"GET /events HTTP/1.1\r\nhost: localhost\r\n\r\n")
        .await?;
    // The stalled client holds the only subscriber slot once registered.
    tokio::time::timeout(WAIT, async {
        loop {
            let response = client().get(url(port, "/events")).send().await?;
            if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                return Ok::<_, anyhow::Error>(());
            }
            drop(response);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    for sequence in 1..=256 {
        sink.handle(input(sequence)?).await?;
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let error = sink
        .stop(&stop_with_deadline(
            StopMode::Drain,
            Instant::now() + Duration::from_millis(200),
        ))
        .await
        .expect_err("a stalled subscriber keeps the drain from finishing");
    assert!(error.is_retryable(), "{error:#}");
    assert!(
        sink.start(&mut contexts::start()).await.is_err(),
        "a retained listener blocks restart"
    );
    // Closing the stalled client lets the retained drain finish.
    drop(stalled);
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    sink.start(&mut contexts::start()).await?;
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn abort_closes_connections_promptly() -> Result<()> {
    let port = free_port();
    let mut sink = started(base(port)).await?;
    let mut subscriber = open(client().get(url(port, "/events"))).await?;
    let started_at = Instant::now();
    sink.stop(&contexts::stop(StopMode::Abort)).await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Ok(Some(_)) = subscriber.frame().await {}
    })
    .await
    .context("abort must close the connection promptly")?;
    assert!(started_at.elapsed() < Duration::from_secs(2));
    let _released = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn pre_cancelled_start_binds_nothing_and_remains_startable() -> Result<()> {
    let port = free_port();
    let mut sink = create_sink(&SseFactory, &component_id(), base(port), &inputs())?;
    sink.initialize(&contexts::initialize()).await?;
    let error = sink
        .start(&mut contexts::cancelled_start())
        .await
        .expect_err("cancelled start");
    assert_eq!(error.kind(), LifecycleErrorKind::Cancelled);
    let _free = std::net::TcpListener::bind(("127.0.0.1", port))?;
    drop(_free);
    sink.stop(&contexts::stop(StopMode::Abort)).await?;
    sink.start(&mut contexts::start()).await?;
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

fn direct_sink(config: Value) -> Result<SseSink> {
    let config = SseSinkConfig::parse(config)?;
    let outputs = SinkOutputs::from_inputs(&inputs(), &config.outputs)?;
    SseSink::new(
        SseFactory.metadata().descriptor(component_id())?,
        config,
        outputs,
        None,
    )
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_cleanup_retains_the_server_and_blocks_restart() -> Result<()> {
    use futures_util::FutureExt;
    let mut sink = direct_sink(base(0))?;
    let (release, wait) = tokio::sync::oneshot::channel::<()>();
    let activation = test_support::activation(&sink.config)?;
    sink.running = Some(Running {
        activation,
        server: tokio::sync::RwLock::new(None),
        address: "127.0.0.1:0".parse()?,
    });
    drasi_computation_contracts::workers::spawn_owned_worker(
        &sink.running.as_ref().context("owner")?.server,
        async {
            let _ = wait.await;
            Ok(())
        },
    )
    .await?;
    assert!(sink
        .stop(&contexts::stop(StopMode::Drain))
        .now_or_never()
        .is_none());
    assert!(sink
        .running
        .as_ref()
        .context("retained owner")?
        .server
        .read()
        .await
        .is_some());
    assert!(sink.start(&mut contexts::start()).await.is_err());
    let _ = release.send(());
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    sink.start(&mut contexts::start()).await?;
    assert!(sink.local_address().is_some());
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

/// A panicked listener was reported when it panicked (through the SDK's
/// owned-worker observation); stopping afterwards succeeds and the sink can
/// start again.
#[tokio::test(flavor = "current_thread")]
async fn stop_after_a_listener_panic_succeeds_and_allows_restart() -> Result<()> {
    let mut sink = direct_sink(base(0))?;
    let activation = test_support::activation(&sink.config)?;
    sink.running = Some(Running {
        activation,
        server: tokio::sync::RwLock::new(None),
        address: "127.0.0.1:0".parse()?,
    });
    let (panicked, observed) = tokio::sync::oneshot::channel::<()>();
    drasi_computation_contracts::workers::spawn_owned_worker(
        &sink.running.as_ref().context("owner")?.server,
        async move {
            let _guard = panicked;
            panic!("injected listener panic");
        },
    )
    .await?;
    // The sender drops when the listener unwinds.
    let _ = observed.await;
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    assert!(sink.running.is_none());
    sink.start(&mut contexts::start()).await?;
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn failed_bind_starts_no_worker_and_remains_retryable() -> Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let mut sink = direct_sink(base(port))?;
    assert!(sink.start(&mut contexts::start()).await.is_err());
    assert!(sink.running.is_none());
    drop(listener);
    sink.start(&mut contexts::start()).await?;
    assert!(sink.start(&mut contexts::start()).await.is_err());
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    sink.dispose(&contexts::dispose(
        drasi_computation_plugin_sdk::lifecycle::DisposeReason::Removal,
        drasi_computation_plugin_sdk::lifecycle::StateIntent::Erase,
        1,
    )?)
    .await?;
    Ok(())
}

#[tokio::test]
async fn lag_and_shutdown_close_subscriptions() -> Result<()> {
    let config = SseSinkConfig::parse(base(0))?;
    let activation = test_support::activation(&config)?;
    let slots = test_support::subscribers(&activation).clone();
    let entry = PathEntry::new(1);
    let frame = |data: &str| Frame {
        id: 0,
        data: Arc::from(data),
    };
    let slot = SubscriberSlot::acquire(&slots, 8).context("slot")?;
    let stream = component::events(
        entry.sender.subscribe(),
        activation.clone(),
        slot,
        Duration::from_secs(30),
        Duration::from_secs(30),
    );
    tokio::pin!(stream);
    // The first heartbeat is immediate.
    assert!(stream.next().await.is_some());
    entry.sender.send(frame("first"))?;
    assert!(stream.next().await.is_some());
    entry.sender.send(frame("second"))?;
    entry.sender.send(frame("third"))?;
    assert!(
        stream.next().await.is_none(),
        "a lagging subscriber is closed"
    );
    // The finished stream released its subscriber slot.
    assert_eq!(
        slots.load(std::sync::atomic::Ordering::Acquire),
        0,
        "slot released"
    );
    let slot = SubscriberSlot::acquire(&slots, 8).context("slot")?;
    let stream = component::events(
        entry.sender.subscribe(),
        activation.clone(),
        slot,
        Duration::from_secs(30),
        Duration::from_secs(30),
    );
    tokio::pin!(stream);
    test_support::cancel(&activation);
    assert!(stream.next().await.is_none());
    Ok(())
}

// ----------------------------------------------------------- admission

#[tokio::test(flavor = "current_thread")]
async fn subscriber_cap_rejects_excess_subscribers() -> Result<()> {
    let port = free_port();
    let mut sink = started(with(base(port), json!({"maxSubscribers": 2}))).await?;
    let client = client();
    let first = open(client.get(url(port, "/events"))).await?;
    let _second = open(client.get(url(port, "/events"))).await?;
    let third = client.get(url(port, "/events")).send().await?;
    assert_eq!(third.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(third.headers()["retry-after"], "1");
    // A disconnect frees its slot.
    drop(first);
    tokio::time::timeout(WAIT, async {
        loop {
            let response = client.get(url(port, "/events")).send().await?;
            if response.status().is_success() {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    sink.stop(&contexts::stop(StopMode::Abort)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn bearer_token_by_header_or_query_protects_events_and_snapshots() -> Result<()> {
    let port = free_port();
    let secondary = "fedcba9876543210-token";
    let mut sink = started(with(
        base(port),
        json!({"authToken": TOKEN, "authTokenSecondary": secondary}),
    ))
    .await?;
    let client = client();
    for path in ["/events", "/snapshot/q"] {
        let response = client.get(url(port, path)).send().await?;
        assert_eq!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "{path}"
        );
        assert_eq!(response.headers()["www-authenticate"], "Bearer");
        let response = client
            .get(url(port, path))
            .bearer_auth("wrong-token-0123456")
            .send()
            .await?;
        assert_eq!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "{path}"
        );
        let response = client
            .get(url(
                port,
                &format!("{path}?access_token=wrong-token-0123456"),
            ))
            .send()
            .await?;
        assert_eq!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }
    let snapshot = client
        .get(url(port, "/snapshot/q"))
        .bearer_auth(TOKEN)
        .send()
        .await?;
    assert_eq!(snapshot.status(), reqwest::StatusCode::NOT_IMPLEMENTED);
    let mut by_header = open(client.get(url(port, "/events")).bearer_auth(TOKEN)).await?;
    let mut by_secondary = open(client.get(url(port, "/events")).bearer_auth(secondary)).await?;
    // Percent-encoded query tokens are decoded.
    let mut by_query =
        open(client.get(url(port, "/events?access_token=0123456789abcdef%2Dtoken"))).await?;
    sink.handle(input(5)?).await?;
    for stream in [&mut by_header, &mut by_secondary, &mut by_query] {
        assert_eq!(stream.data().await?.json().context("json")?["sequence"], 5);
    }
    sink.stop(&contexts::stop(StopMode::Drain)).await?;

    // allowQueryToken: false accepts only the header.
    let port = free_port();
    let mut sink = started(with(
        base(port),
        json!({"authToken": TOKEN, "allowQueryToken": false}),
    ))
    .await?;
    let response = client
        .get(url(port, &format!("/events?access_token={TOKEN}")))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(client
        .get(url(port, "/events"))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .status()
        .is_success());
    sink.stop(&contexts::stop(StopMode::Abort)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn rate_limit_rejects_bursts_of_new_connections() -> Result<()> {
    let port = free_port();
    let mut sink = started(with(
        base(port),
        json!({"rateLimitPerSecond": 1, "rateLimitBurst": 2}),
    ))
    .await?;
    let client = client();
    let mut statuses = Vec::new();
    for _ in 0..3 {
        statuses.push(client.get(url(port, "/missing")).send().await?.status());
    }
    assert_eq!(statuses[..2], [reqwest::StatusCode::NOT_FOUND; 2]);
    assert_eq!(statuses[2], reqwest::StatusCode::TOO_MANY_REQUESTS);
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cors_is_off_by_default_and_follows_configured_origins() -> Result<()> {
    let client = client();
    let origin = "https://app.example";
    // Default: no CORS headers on responses or preflights.
    let port = free_port();
    let mut sink = started(base(port)).await?;
    let response = client
        .get(url(port, "/events"))
        .header("origin", origin)
        .send()
        .await?;
    assert!(response.status().is_success());
    assert!(response
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    let preflight = client
        .request(reqwest::Method::OPTIONS, url(port, "/events"))
        .header("origin", origin)
        .send()
        .await?;
    assert_eq!(preflight.status(), reqwest::StatusCode::NO_CONTENT);
    assert!(preflight
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    sink.stop(&contexts::stop(StopMode::Abort)).await?;

    // Listed origins are echoed; others get nothing.
    let port = free_port();
    let mut sink = started(with(base(port), json!({"corsAllowedOrigins": [origin]}))).await?;
    let response = client
        .get(url(port, "/events"))
        .header("origin", origin)
        .send()
        .await?;
    assert_eq!(response.headers()["access-control-allow-origin"], origin);
    assert_eq!(response.headers()["vary"], "origin");
    let response = client
        .get(url(port, "/events"))
        .header("origin", "https://evil.example")
        .send()
        .await?;
    assert!(response
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    let preflight = client
        .request(reqwest::Method::OPTIONS, url(port, "/snapshot/q"))
        .header("origin", origin)
        .send()
        .await?;
    assert_eq!(preflight.headers()["access-control-allow-origin"], origin);
    assert_eq!(
        preflight.headers()["access-control-allow-methods"],
        "GET, OPTIONS"
    );
    let snapshot = client
        .get(url(port, "/snapshot/q"))
        .header("origin", origin)
        .send()
        .await?;
    assert_eq!(
        snapshot.headers()["access-control-expose-headers"],
        "x-drasi-output-epoch, x-drasi-output-position"
    );
    sink.stop(&contexts::stop(StopMode::Abort)).await?;

    // An explicit wildcard.
    let port = free_port();
    let mut sink = started(with(base(port), json!({"corsAllowedOrigins": ["*"]}))).await?;
    let response = client.get(url(port, "/events")).send().await?;
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    sink.stop(&contexts::stop(StopMode::Abort)).await?;
    Ok(())
}

#[test]
fn configuration_rejects_invalid_unsafe_and_unsupported_options() {
    for value in [
        json!({"outputs": {"bad id": {"component": OUTPUT_COMPONENT_ID}}}),
        json!({"queryStreams": {"q": "q/out"}}),
        json!({"producers": {"q": "q/out"}}),
        json!({"outputs": {"q": {"component": OUTPUT_COMPONENT_ID, "stream": "q/out"}}}),
        json!({"outputs": q_outputs(), "heartbeatIntervalMs": 0}),
        json!({"outputs": q_outputs(), "keepAliveIntervalMs": 0}),
        json!({"outputs": q_outputs(), "broadcastCapacity": 0}),
        json!({"outputs": q_outputs(), "ssePath": "events"}),
        json!({"outputs": q_outputs(), "ssePath": "/snapshot/x"}),
        json!({"outputs": q_outputs(), "maxPaths": 0}),
        json!({"outputs": q_outputs(), "maxSubscribers": 0}),
        json!({"outputs": q_outputs(), "maxConnections": 0}),
        json!({"outputs": q_outputs(), "headerReadTimeoutMs": 0}),
        json!({"outputs": q_outputs(), "routes": {"q": {"added": {"templat": "typo"}}}}),
        json!({"outputs": q_outputs(), "host": "localhost"}),
        // Non-loopback exposure needs a token or an explicit opt-in.
        json!({"outputs": q_outputs(), "host": "0.0.0.0"}),
        json!({"outputs": q_outputs(), "host": "192.168.1.10"}),
        json!({"outputs": q_outputs(), "authToken": "short"}),
        json!({"outputs": q_outputs(), "authTokenSecondary": TOKEN}),
        json!({"outputs": q_outputs(), "rateLimitBurst": 4}),
        json!({"outputs": q_outputs(), "tlsCertificatePem": "not pem"}),
        json!({"outputs": q_outputs(), "corsAllowedOrigins": ["*", "https://a.example"]}),
        json!({"outputs": q_outputs(), "corsAllowedOrigins": ["https://a.example/path"]}),
        json!({"outputs": q_outputs(), "corsAllowedOrigins": ["a.example"]}),
        json!({"outputs": q_outputs(), "corsAllowedOrigins": ["https://a.example", "https://A.example"]}),
    ] {
        assert!(SseSinkConfig::parse(value.clone()).is_err(), "{value}");
    }
    for value in [
        json!({"host": "0.0.0.0", "authToken": TOKEN}),
        json!({"host": "0.0.0.0", "allowUnauthenticated": true}),
        json!({"host": "::1"}),
    ] {
        assert!(SseSinkConfig::parse(value.clone()).is_ok(), "{value}");
    }
    assert_eq!(
        SseSinkConfig::parse(json!({})).expect("defaults").host,
        "127.0.0.1"
    );
}

#[test]
fn routes_must_name_an_incoming_output() -> Result<()> {
    let config = SseSinkConfig::parse(json!({"outputs": q_outputs(), "routes": {"missing": {}}}))?;
    let outputs = SinkOutputs::from_inputs(&inputs(), &config.outputs)?;
    assert!(config.validate_outputs(&outputs).is_err());
    assert!(create_sink(
        &SseFactory,
        &component_id(),
        json!({"outputs": q_outputs(), "routes": {"missing": {}}}),
        &inputs()
    )
    .is_err());
    let config = SseSinkConfig::parse(json!({"outputs": q_outputs(), "routes": {"q": {}}}))?;
    let outputs = SinkOutputs::from_inputs(&inputs(), &config.outputs)?;
    config.validate_outputs(&outputs)?;
    assert_eq!(outputs.aliases().collect::<Vec<_>>(), ["q"]);
    Ok(())
}

#[test]
fn query_tokens_are_found_and_decoded() {
    use component::query_token;
    assert_eq!(
        query_token(Some("a=1&access_token=x%2By")),
        Some("x+y".into())
    );
    assert_eq!(query_token(Some("access_token=abc")), Some("abc".into()));
    assert_eq!(query_token(Some("access_token=%zz")), None);
    assert_eq!(query_token(Some("token=abc")), None);
    assert_eq!(query_token(None), None);
}

// ------------------------------------------------------------ transport

#[tokio::test(flavor = "current_thread")]
async fn tls_listener_serves_event_streams() -> Result<()> {
    let server = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let port = free_port();
    let mut sink = started(with(
        base(port),
        json!({
            "tlsCertificatePem": server.cert.pem(),
            "tlsPrivateKeyPem": server.key_pair.serialize_pem(),
        }),
    ))
    .await?;
    let tls = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(
            server.cert.pem().as_bytes(),
        )?)
        .resolve(
            "localhost",
            std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        )
        .connect_timeout(WAIT)
        .build()?;
    let mut stream = open(tls.get(format!("https://localhost:{port}/events"))).await?;
    sink.handle(input(9)?).await?;
    assert_eq!(stream.data().await?.json().context("json")?["sequence"], 9);
    let plain = tokio::time::timeout(WAIT, client().get(url(port, "/events")).send()).await?;
    assert!(
        plain
            .map(|response| !response.status().is_success())
            .unwrap_or(true),
        "plaintext must not be served by a TLS listener"
    );
    sink.stop(&contexts::stop(StopMode::Abort)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn keep_alive_comments_and_event_ids_are_sent() -> Result<()> {
    let port = free_port();
    let mut sink = started(with(
        base(port),
        json!({"keepAliveIntervalMs": 20, "eventIds": true}),
    ))
    .await?;
    let mut stream = open(client().get(url(port, "/events"))).await?;
    tokio::time::timeout(WAIT, async {
        loop {
            let frame = stream.frame().await?.context("stream ended")?;
            if frame.comments.iter().any(|comment| comment == "keep-alive") {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    sink.handle(input(1)?).await?;
    sink.handle(input(2)?).await?;
    assert_eq!(stream.data().await?.id.as_deref(), Some("1"));
    assert_eq!(stream.data().await?.id.as_deref(), Some("2"));
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn carriage_returns_in_templates_are_normalised() -> Result<()> {
    let port = free_port();
    let mut sink = started(with(
        base(port),
        json!({"routes": {"q": {"added": {"template": "a\r\nb\rc"}}}}),
    ))
    .await?;
    let mut stream = open(client().get(url(port, "/events"))).await?;
    sink.handle(input(1)?).await?;
    assert_eq!(stream.data().await?.data.as_deref(), Some("a\nb\nc"));
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn idle_dynamic_paths_are_pruned_at_the_path_bound() -> Result<()> {
    let port = free_port();
    let mut sink = started(with(
        base(port),
        json!({
            "maxPaths": 2,
            "routes": {"q": {"added": {"template": "", "path": "items/{{identity.id}}"}}},
        }),
    ))
    .await?;
    let client = client();
    sink.handle(input(1)?).await?;
    assert!(client
        .get(url(port, "/events/items/e1"))
        .send()
        .await?
        .status()
        .is_success());
    // e1 is idle again (the probe response above was dropped), so e2 replaces it.
    tokio::time::sleep(Duration::from_millis(50)).await;
    sink.handle(input(2)?).await?;
    assert_eq!(
        client
            .get(url(port, "/events/items/e1"))
            .send()
            .await?
            .status(),
        reqwest::StatusCode::NOT_FOUND,
        "a pruned dynamic path returns 404 until its next event"
    );
    let mut subscriber = open(client.get(url(port, "/events/items/e2"))).await?;
    // e2 has a subscriber, so a third dynamic path exceeds the bound.
    assert!(sink.handle(input(3)?).await.is_err());
    // The static base path is never pruned.
    assert!(client
        .get(url(port, "/events"))
        .send()
        .await?
        .status()
        .is_success());
    drop(subscriber.frame().await?);
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn handle_requires_a_started_listener() -> Result<()> {
    let mut sink = create_sink(&SseFactory, &component_id(), base(free_port()), &inputs())?;
    assert!(sink.handle(input(1)?).await.is_err());
    sink.initialize(&contexts::initialize()).await?;
    sink.start(&mut contexts::start()).await?;
    sink.handle(input(1)?).await?;
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    assert!(sink.handle(input(2)?).await.is_err());
    Ok(())
}
