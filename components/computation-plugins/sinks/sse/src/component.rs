// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_plugin_sdk::{NativeComponent, NativeSink};

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use axum::{
    http::{header, StatusCode},
    response::{sse::Event, IntoResponse, Sse},
    routing::get,
    Router,
};
use drasi_computation_contracts::{
    workers::{join_owned_worker_gracefully, spawn_owned_worker, WorkerCompletion},
    *,
};
use drasi_computation_plugin_sdk::{
    Capabilities, Component, ControlSender, CreateRequest, CreatedComponent, Factory,
    FactoryMetadata, NativeOutputSnapshot, NativeOutputSnapshots,
};
use drasi_reaction_common::sse::{default_changes_payload, QueryConfig, SseRenderer};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    convert::Infallible,
    net::IpAddr,
    sync::Arc,
    time::Duration,
};
use tokio::{net::TcpListener, sync::broadcast, sync::RwLock, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const MAX_BROADCAST_CAPACITY: usize = 1_048_576;
const MAX_PATHS: usize = 65_536;

fn host() -> String {
    "0.0.0.0".into()
}
fn port() -> u16 {
    8080
}
fn path() -> String {
    "/events".into()
}
fn heartbeat() -> u64 {
    30_000
}
fn capacity() -> usize {
    1024
}
fn max_paths() -> usize {
    1024
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SseSinkConfig {
    #[serde(default, skip_serializing_if = "OutputAliases::is_empty")]
    pub outputs: OutputAliases,
    #[serde(default = "host")]
    pub host: String,
    #[serde(default = "port")]
    pub port: u16,
    #[serde(default = "path")]
    pub sse_path: String,
    #[serde(default = "heartbeat")]
    pub heartbeat_interval_ms: u64,
    #[serde(default = "capacity")]
    pub broadcast_capacity: usize,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub routes: HashMap<String, QueryConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig>,
    #[serde(default = "max_paths")]
    pub max_paths: usize,
}
impl SseSinkConfig {
    pub fn definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
        use drasi_computation_plugin_sdk::{
            ConfigDefinition, ConfigOption as Field, ConfigType::*,
        };
        ConfigDefinition::try_new([
            Field::new("outputs", Object, false, "Optional aliases for incoming outputs: a map of alias to {component, port?}. Every incoming graph edge is accepted whether or not it is aliased; the graph supplies the producing component, output port and stream. An unaliased output is named after its producing component. The alias names the output in routes and snapshot URLs."),
            Field::new("host", String, false, "IP address on which to listen.").default(host()),
            Field::new("port", Integer, false, "TCP port; zero requests an available local port.").default(port()).minimum(0).maximum(u16::MAX as u64),
            Field::new("ssePath", String, false, "Absolute event-stream path without query, fragment or control characters; /snapshot and its descendants are reserved.").default(path()),
            Field::new("heartbeatIntervalMs", Integer, false, "Positive heartbeat interval in milliseconds, representable by the system clock.").default(heartbeat()).minimum(1),
            Field::new("broadcastCapacity", Integer, false, "Maximum retained notifications in the shared broadcast buffer; a lagging client is disconnected and must obtain a fresh snapshot.").default(capacity()).minimum(1).maximum(MAX_BROADCAST_CAPACITY as u64),
            Field::new("routes", Object, false, "Per-output-alias added/updated/deleted body templates and optional static or templated paths; full aliases take precedence over their last dotted segment.").default(serde_json::json!({})),
            Field::new("defaultTemplate", Object, false, "Fallback operation templates for outputs without a matching route; an empty body uses the default graph-change SSE payload."),
            Field::new("maxPaths", Integer, false, "Maximum number of configured and dynamically rendered event paths, including the base path; exceeding the bound fails before broadcasting the input.").default(max_paths()).minimum(1).maximum(MAX_PATHS as u64),
        ]).expect("constant SSE sink configuration")
    }

    pub fn parse(value: Value) -> Result<Self> {
        if let Some(routes) = value.get("routes").and_then(Value::as_object) {
            for config in routes.values() {
                validate_template_fields(config)?;
            }
        }
        if let Some(config) = value
            .get("defaultTemplate")
            .filter(|value| !value.is_null())
        {
            validate_template_fields(config)?;
        }
        let config: Self = serde_json::from_value(value)?;
        config.outputs.validate()?;
        config.host.parse::<IpAddr>()?;
        ensure!(
            valid_path(&config.sse_path),
            "ssePath must be an absolute HTTP path without query, fragment or control characters and outside /snapshot"
        );
        crate::config::duration(config.heartbeat_interval_ms, "heartbeatIntervalMs")?;
        ensure!(
            (1..=MAX_BROADCAST_CAPACITY).contains(&config.broadcast_capacity),
            "broadcastCapacity must be in 1..=1048576"
        );
        ensure!(
            (1..=MAX_PATHS).contains(&config.max_paths),
            "maxPaths must be in 1..=65536"
        );
        SseRenderer::validate(
            &config.routes,
            config.default_template.as_ref(),
            &config.routes.keys().cloned().collect::<Vec<_>>(),
        )?;
        let paths = config.renderer().static_paths();
        ensure!(
            paths.len() <= config.max_paths && paths.iter().all(|path| valid_path(path)),
            "configured SSE paths are invalid or exceed maxPaths"
        );
        Ok(config)
    }

    /// Checks that depend on the incoming edges: routes must name an output of
    /// this sink.
    fn validate_outputs(&self, outputs: &SinkOutputs) -> Result<()> {
        SseRenderer::validate(
            &self.routes,
            self.default_template.as_ref(),
            &outputs.aliases().map(str::to_owned).collect::<Vec<_>>(),
        )?;
        Ok(())
    }

    fn renderer(&self) -> SseRenderer {
        SseRenderer::new(
            self.sse_path.clone(),
            self.routes.clone(),
            self.default_template.clone(),
        )
    }
}

fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains(['?', '#'])
        && !path.chars().any(char::is_control)
        && path != "/snapshot"
        && !path.starts_with("/snapshot/")
}

fn validate_template_fields(config: &Value) -> Result<()> {
    for (operation, spec) in config
        .as_object()
        .context("SSE templates must be an object")?
    {
        ensure!(
            matches!(operation.as_str(), "added" | "updated" | "deleted"),
            "unsupported SSE template operation: {operation}"
        );
        if spec.is_null() {
            continue;
        }
        for field in spec
            .as_object()
            .context("SSE operation template must be an object")?
            .keys()
        {
            ensure!(
                matches!(field.as_str(), "template" | "path"),
                "unsupported SSE template field: {field}"
            );
        }
    }
    Ok(())
}

pub struct SseFactory;
impl Factory for SseFactory {
    fn supports_output_snapshots(&self) -> bool {
        true
    }
    fn configuration_details(&self) -> Option<drasi_computation_plugin_sdk::ConfigDetails> {
        Some(SseSinkConfig::definition().details())
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(crate::SSE_SINK, "1")
                .expect("constant SSE implementation"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: SseSinkConfig::definition().schema(),
            ports: vec![PortDescriptor::new(
                PortId::try_new("in").expect("constant port"),
                PortDirection::Input,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
            completion: Some(SinkCompletion::Accepted),
            capabilities: Capabilities {
                binds_inputs: true,
                ..Capabilities::default()
            },
        }
    }
    fn create(&self, request: &CreateRequest, control: ControlSender) -> Result<CreatedComponent> {
        self.create_with_output_snapshots(request, control, None)
    }
    fn create_with_output_snapshots(
        &self,
        request: &CreateRequest,
        _: ControlSender,
        snapshots: Option<NativeOutputSnapshots>,
    ) -> Result<CreatedComponent> {
        let config = SseSinkConfig::parse(request.configuration.clone())?;
        let outputs = SinkOutputs::from_inputs(&request.inputs, &config.outputs)?;
        config.validate_outputs(&outputs)?;
        let renderer = config.renderer();
        Ok(Component::Sink(Box::new(SseSink {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            config,
            outputs,
            renderer,
            snapshots,
            running: None,
        }))
        .into())
    }
}

struct Running {
    broadcasters: Arc<RwLock<BTreeMap<String, broadcast::Sender<String>>>>,
    cancel: CancellationToken,
    server: RwLock<Option<JoinHandle<std::io::Result<()>>>>,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct SseSink {
    descriptor: ComponentDescriptor,
    config: SseSinkConfig,
    outputs: SinkOutputs,
    renderer: SseRenderer,
    snapshots: Option<NativeOutputSnapshots>,
    running: Option<Running>,
}

const OUTPUT_EPOCH_HEADER: &str = "x-drasi-output-epoch";
const OUTPUT_POSITION_HEADER: &str = "x-drasi-output-position";

/// The HTTP outcome of a failed snapshot read. An absent capability is explicit
/// and never an empty snapshot.
fn snapshot_failure(error: &anyhow::Error) -> (StatusCode, &'static str) {
    use drasi_computation_plugin_sdk::{abi::status, transport::Failure};
    if let Some(read) = error.downcast_ref::<OutputReadError>() {
        return match read {
            OutputReadError::Absent { .. } => (
                StatusCode::NOT_IMPLEMENTED,
                "The output does not provide a snapshot",
            ),
            OutputReadError::UnknownOutput(_) | OutputReadError::UnknownComponent(_) => {
                (StatusCode::NOT_FOUND, "Unknown output")
            }
            OutputReadError::Invalid(_) | OutputReadError::Failed(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The output snapshot failed",
            ),
            _ => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The output snapshot is not ready",
            ),
        };
    }
    match error.downcast_ref::<Failure>().map(|failure| failure.code) {
        Some(status::BUSY) => (StatusCode::TOO_MANY_REQUESTS, "Snapshot readers are busy"),
        Some(status::INVALID_ARGUMENT) => (
            StatusCode::NOT_FOUND,
            "The output is outside the snapshot capability",
        ),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The output snapshot is unavailable",
        ),
    }
}

async fn snapshot_response(
    alias: String,
    outputs: Arc<SinkOutputs>,
    snapshots: Option<NativeOutputSnapshots>,
    stopped: CancellationToken,
) -> axum::response::Response {
    let response = async {
        let Some(binding) = outputs.get(&alias) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let Some(snapshots) = snapshots else {
            return (
                StatusCode::NOT_IMPLEMENTED,
                "This sink has no output snapshot capability bound",
            )
                .into_response();
        };
        let endpoint = binding.endpoint();
        let schema = GraphChangeCodec::schema();
        let snapshot = tokio::select! {
            biased;
            _ = stopped.cancelled() => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            snapshot = snapshots.snapshot(&endpoint, &schema) => match snapshot {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    eprintln!("drasi.network SSE snapshot for {endpoint} failed: {error:#}");
                    return snapshot_failure(&error).into_response();
                }
            },
        };
        let epoch = snapshot.cursor.epoch().to_string();
        let position = snapshot.cursor.sequence();
        let mut response =
            axum::body::Body::from_stream(snapshot_body(snapshot, stopped)).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            "application/json".parse().expect("constant header"),
        );
        response.headers_mut().insert(
            OUTPUT_EPOCH_HEADER,
            epoch.parse().expect("UUID is a valid header value"),
        );
        response
            .headers_mut()
            .insert(OUTPUT_POSITION_HEADER, position.into());
        response
    }
    .await;
    let mut response = response;
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        "*".parse().expect("constant header"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        format!("{OUTPUT_EPOCH_HEADER},{OUTPUT_POSITION_HEADER}")
            .parse()
            .expect("constant header"),
    );
    response
}

/// The snapshot as a JSON array of canonical `ADD` changes, each with its real
/// identity, element shape and complete `after` image: the same change shape a
/// live frame carries, so a client applies both with one identity-keyed state.
fn snapshot_body(
    snapshot: NativeOutputSnapshot,
    stopped: CancellationToken,
) -> impl futures_util::Stream<Item = std::result::Result<String, std::io::Error>> + Send {
    let endpoint = snapshot.endpoint.clone();
    let mut records = snapshot.records;
    async_stream::try_stream! {
        use futures_util::StreamExt;
        yield String::from("[");
        let mut ordinal = 0u64;
        loop {
            let record = tokio::select! {
                biased;
                _ = stopped.cancelled() => Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "SSE snapshot stopped",
                )),
                record = records.next() => Ok(record),
            }?;
            let Some(record) = record else { break; };
            let encoded = record
                .and_then(|record| {
                    let change = GraphChange::from_operation(&ChangeOperation::Added {
                        ordinal,
                        after: record,
                    })?;
                    Ok(change.to_json()?.to_string())
                })
                .map_err(|error| {
                    eprintln!("drasi.network SSE snapshot stream for {endpoint} failed: {error:#}");
                    std::io::Error::other("SSE output snapshot stream failed")
                })?;
            yield if ordinal == 0 { encoded } else { format!(",{encoded}") };
            ordinal += 1;
        }
        yield String::from("]");
    }
}

fn events(
    mut receiver: broadcast::Receiver<String>,
    cancel: CancellationToken,
    heartbeat_interval: Duration,
) -> impl futures_util::Stream<Item = std::result::Result<Event, Infallible>> {
    async_stream::stream! {
        let mut heartbeat = tokio::time::interval(heartbeat_interval);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let message = tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                message = receiver.recv() => match message {
                    Ok(message) => message,
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        eprintln!("drasi.network SSE subscriber lagged by {count} events; closing for reconnect and snapshot");
                        break;
                    }
                },
                _ = heartbeat.tick() => json!({
                    "type": "heartbeat", "ts": chrono::Utc::now().timestamp_millis()
                }).to_string(),
            };
            yield Ok(Event::default().data(message));
        }
    }
}

#[async_trait]
impl NativeComponent for SseSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<Value> {
        Ok(serde_json::to_value(&self.config)?)
    }
    async fn start(&mut self) -> Result<()> {
        ensure!(
            self.running.is_none(),
            "SSE sink is started or still needs cleanup"
        );
        let listener = TcpListener::bind((self.config.host.as_str(), self.config.port))
            .await
            .context("bind native SSE listener")?;
        let broadcasters = Arc::new(RwLock::new(
            self.renderer
                .static_paths()
                .into_iter()
                .map(|path| (path, broadcast::channel(self.config.broadcast_capacity).0))
                .collect::<BTreeMap<_, _>>(),
        ));
        let cancel = CancellationToken::new();
        let subscriptions = broadcasters.clone();
        let stopped = cancel.clone();
        let interval =
            crate::config::duration(self.config.heartbeat_interval_ms, "heartbeatIntervalMs")?;
        let snapshot_service = self.snapshots.clone();
        let snapshot_outputs = Arc::new(self.outputs.clone());
        let snapshot_stop = cancel.clone();
        let router = Router::new()
            .route(
                "/snapshot/:alias",
                get(
                    move |axum::extract::Path(alias): axum::extract::Path<String>| {
                        snapshot_response(
                            alias,
                            snapshot_outputs.clone(),
                            snapshot_service.clone(),
                            snapshot_stop.clone(),
                        )
                    },
                )
                .options(|| async {
                    (
                        [
                            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
                            (header::ACCESS_CONTROL_ALLOW_METHODS, "GET, OPTIONS"),
                            (header::ACCESS_CONTROL_ALLOW_HEADERS, "*"),
                        ],
                        StatusCode::NO_CONTENT,
                    )
                }),
            )
            .fallback(
                get(move |request: axum::extract::Request| {
                    let subscriptions = subscriptions.clone();
                    let stopped = stopped.clone();
                    let path = request.uri().path().to_owned();
                    async move {
                        let receiver = subscriptions
                            .read()
                            .await
                            .get(&path)
                            .map(broadcast::Sender::subscribe);
                        let mut response = match receiver {
                            Some(receiver) => {
                                Sse::new(events(receiver, stopped, interval)).into_response()
                            }
                            None => StatusCode::NOT_FOUND.into_response(),
                        };
                        response.headers_mut().insert(
                            header::ACCESS_CONTROL_ALLOW_ORIGIN,
                            "*".parse().expect("constant header"),
                        );
                        response
                    }
                })
                .options(|| async {
                    (
                        [
                            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
                            (header::ACCESS_CONTROL_ALLOW_METHODS, "GET, OPTIONS"),
                            (header::ACCESS_CONTROL_ALLOW_HEADERS, "*"),
                        ],
                        StatusCode::NO_CONTENT,
                    )
                }),
            );
        eprintln!(
            "drasi.network SSE listener started: component={} address={}",
            self.descriptor.id(),
            listener.local_addr()?
        );
        self.running = Some(Running {
            broadcasters,
            cancel: cancel.clone(),
            server: RwLock::new(None),
        });
        spawn_owned_worker(
            &self.running.as_ref().expect("installed owner").server,
            async move {
                let _stopped = cancel.clone().drop_guard();
                let result = axum::serve(listener, router)
                    .with_graceful_shutdown(cancel.clone().cancelled_owned())
                    .await;
                if !cancel.is_cancelled() {
                    eprintln!("drasi.network SSE listener ended unexpectedly: {result:?}");
                }
                result
            },
        )
        .await?;
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        let Some(running) = &self.running else {
            return Ok(());
        };
        running.cancel.cancel();
        if let WorkerCompletion::Completed(result) =
            join_owned_worker_gracefully(&mut *running.server.write().await, Duration::from_secs(5))
                .await?
        {
            result.context("native SSE server failed")?;
        }
        self.running = None;
        Ok(())
    }
}
#[async_trait]
impl NativeSink for SseSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Accepted
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        let running = self.running.as_ref().context("SSE sink has not started")?;
        ensure!(
            !running.cancel.is_cancelled(),
            "SSE listener stopped; cleanup required"
        );
        let notifications = notifications(&input, &self.outputs)?;
        let events = self.renderer.render_changes(
            &notifications,
            chrono::Utc::now().timestamp_millis(),
            |message| eprintln!("drasi.network SSE {}: {message}", self.descriptor.id()),
        )?;
        ensure!(
            events.iter().all(|event| valid_path(&event.path)),
            "rendered SSE path must be absolute without query, fragment or control characters and outside /snapshot"
        );
        let mut broadcasters = running.broadcasters.write().await;
        let new_paths: BTreeSet<_> = events
            .iter()
            .map(|event| &event.path)
            .filter(|path| !broadcasters.contains_key(*path))
            .collect();
        ensure!(
            broadcasters.len() + new_paths.len() <= self.config.max_paths,
            "rendered SSE paths exceed maxPaths"
        );
        for path in new_paths {
            broadcasters.insert(
                path.clone(),
                broadcast::channel(self.config.broadcast_capacity).0,
            );
        }
        // Volatile UI notifications require no listener or browser acknowledgement.
        for event in events {
            let _ = broadcasters
                .get(&event.path)
                .expect("validated SSE route")
                .send(event.payload);
        }
        Ok(())
    }
}

/// The default browser frame an input produces without any route template.
pub fn sse_payload(input: &InputEnvelope, outputs: &SinkOutputs) -> Result<Value> {
    let notifications = notifications(input, outputs)?;
    let first = notifications
        .first()
        .context("SSE input has no graph changes")?;
    let changes = notifications
        .iter()
        .map(|notification| notification.change().to_json())
        .collect::<Result<Vec<_>, _>>()?;
    Ok(default_changes_payload(
        first.output_json(),
        first.sequence(),
        chrono::Utc::now().timestamp_millis(),
        changes,
    ))
}

/// Decode standard graph changes from any bound output. The binding names the
/// data and no query annotation is read. Patches and identity-only deletes are
/// sent as such: the browser's row state owns applying them. An envelope
/// without changes yields none; a scheduling control or recovery frame fails.
fn notifications(input: &InputEnvelope, outputs: &SinkOutputs) -> Result<Vec<ChangeNotification>> {
    drasi_computation_network_common::wire::notifications(input, outputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{FutureExt, StreamExt};

    fn q_outputs() -> Value {
        json!({"q": {"component": "q", "port": "out"}})
    }

    fn q_inputs() -> Result<Vec<InputBinding>> {
        Ok(vec![InputBinding {
            input_port: PortId::try_new("in")?,
            output: OutputEndpoint::new(ComponentId::try_new("q")?, PortId::try_new("out")?),
            stream: StreamId::try_new("q/out")?,
        }])
    }

    fn sink() -> Result<SseSink> {
        let config = SseSinkConfig::parse(json!({
            "outputs": q_outputs(), "host": "127.0.0.1", "port": 0,
        }))?;
        Ok(SseSink {
            descriptor: SseFactory
                .metadata()
                .descriptor(ComponentId::try_new("ui")?)?,
            outputs: SinkOutputs::from_inputs(&q_inputs()?, &config.outputs)?,
            renderer: config.renderer(),
            snapshots: None,
            config,
            running: None,
        })
    }

    #[test]
    fn configuration_rejects_invalid_and_unsupported_options() {
        for value in [
            json!({"outputs": {"bad id": {"component": "q"}}}),
            json!({"queryStreams": {"q": "q/out"}}),
            json!({"producers": {"q": "q/out"}}),
            json!({"outputs": {"q": {"component": "q", "stream": "q/out"}}}),
            json!({"outputs": q_outputs(), "heartbeatIntervalMs": 0}),
            json!({"outputs": q_outputs(), "broadcastCapacity": 0}),
            json!({"outputs": q_outputs(), "ssePath": "events"}),
            json!({"outputs": q_outputs(), "maxPaths": 0}),
            json!({"outputs": q_outputs(), "routes": {"q": {"added": {"templat": "typo"}}}}),
        ] {
            assert!(SseSinkConfig::parse(value).is_err());
        }
    }

    #[test]
    fn routes_must_name_an_incoming_output() -> Result<()> {
        let config = SseSinkConfig::parse(json!({"routes": {"missing": {}}}))?;
        let outputs = SinkOutputs::from_inputs(&q_inputs()?, &config.outputs)?;
        assert!(config.validate_outputs(&outputs).is_err());
        let config = SseSinkConfig::parse(json!({"routes": {"q": {}}}))?;
        let outputs = SinkOutputs::from_inputs(&q_inputs()?, &config.outputs)?;
        config.validate_outputs(&outputs)?;
        assert_eq!(outputs.aliases().collect::<Vec<_>>(), ["q"]);
        Ok(())
    }

    #[tokio::test]
    async fn lag_and_shutdown_close_subscriptions() -> Result<()> {
        let (sender, receiver) = broadcast::channel(1);
        let cancel = CancellationToken::new();
        let stream = events(receiver, cancel.clone(), Duration::from_secs(30));
        tokio::pin!(stream);
        sender.send("first".into())?;
        assert!(stream.next().await.is_some());
        sender.send("second".into())?;
        sender.send("third".into())?;
        assert!(stream.next().await.is_none());
        let stream = events(sender.subscribe(), cancel.clone(), Duration::from_secs(30));
        tokio::pin!(stream);
        cancel.cancel();
        assert!(stream.next().await.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_cleanup_retains_the_server_and_blocks_restart() -> Result<()> {
        let mut sink = sink()?;
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        let (sender, _) = broadcast::channel(1);
        sink.running = Some(Running {
            broadcasters: Arc::new(RwLock::new(BTreeMap::from([("/events".into(), sender)]))),
            cancel: CancellationToken::new(),
            server: RwLock::new(None),
        });
        spawn_owned_worker(&sink.running.as_ref().expect("owner").server, async {
            wait.await.expect("release server");
            Ok(())
        })
        .await?;
        assert!(sink.stop().now_or_never().is_none());
        assert!(sink
            .running
            .as_ref()
            .expect("retained owner")
            .server
            .read()
            .await
            .is_some());
        assert!(sink.start().await.is_err());
        release.send(()).expect("server still owned");
        sink.stop().await?;
        sink.start().await?;
        sink.stop().await?;
        Ok(())
    }

    #[tokio::test]
    async fn failed_bind_starts_no_worker_and_remains_retryable() -> Result<()> {
        let mut sink = sink()?;
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        sink.config.port = listener.local_addr()?.port();
        assert!(sink.start().await.is_err());
        assert!(sink.running.is_none());
        drop(listener);
        sink.start().await?;
        assert!(sink.start().await.is_err());
        sink.stop().await?;
        Ok(())
    }
}
