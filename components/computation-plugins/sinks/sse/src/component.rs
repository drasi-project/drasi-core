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
    FactoryMetadata, NativeQuerySnapshot, NativeQuerySnapshots,
};
use drasi_reaction_common::sse::{default_payload, QueryConfig, SseRenderer};
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
    pub query_streams: BTreeMap<String, StreamId>,
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
            Field::new("queryStreams", Object, true, "Nonempty map from query identifiers to their connected output stream identifiers."),
            Field::new("host", String, false, "IP address on which to listen.").default(host()),
            Field::new("port", Integer, false, "TCP port; zero requests an available local port.").default(port()).minimum(0).maximum(u16::MAX as u64),
            Field::new("ssePath", String, false, "Absolute event-stream path without query, fragment or control characters; /snapshot and its descendants are reserved.").default(path()),
            Field::new("heartbeatIntervalMs", Integer, false, "Positive heartbeat interval in milliseconds, representable by the system clock.").default(heartbeat()).minimum(1),
            Field::new("broadcastCapacity", Integer, false, "Maximum retained notifications in the shared broadcast buffer; a lagging client is disconnected and must obtain a fresh snapshot.").default(capacity()).minimum(1).maximum(MAX_BROADCAST_CAPACITY as u64),
            Field::new("routes", Object, false, "Per-query added/updated/deleted body templates and optional static or templated paths; full query identifiers take precedence over their last dotted segment.").default(serde_json::json!({})),
            Field::new("defaultTemplate", Object, false, "Fallback operation templates for queries without a matching route; an empty body uses the existing singular-result SSE payload."),
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
        ensure!(
            !config.query_streams.is_empty(),
            "queryStreams must not be empty"
        );
        for query in config.query_streams.keys() {
            ComponentId::try_new(query.as_str())?;
        }
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
            &config.query_streams.keys().cloned().collect::<Vec<_>>(),
        )?;
        let paths = config.renderer().static_paths();
        ensure!(
            paths.len() <= config.max_paths && paths.iter().all(|path| valid_path(path)),
            "configured SSE paths are invalid or exceed maxPaths"
        );
        Ok(config)
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
    fn supports_query_snapshots(&self) -> bool {
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
                QueryChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
            completion: Some(SinkCompletion::Accepted),
            capabilities: Capabilities::default(),
        }
    }
    fn create(&self, request: &CreateRequest, control: ControlSender) -> Result<CreatedComponent> {
        self.create_with_query_snapshots(request, control, None)
    }
    fn create_with_query_snapshots(
        &self,
        request: &CreateRequest,
        _: ControlSender,
        snapshots: Option<NativeQuerySnapshots>,
    ) -> Result<CreatedComponent> {
        let config = SseSinkConfig::parse(request.configuration.clone())?;
        let renderer = config.renderer();
        Ok(Component::Sink(Box::new(SseSink {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            config,
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
    renderer: SseRenderer,
    snapshots: Option<NativeQuerySnapshots>,
    running: Option<Running>,
}

async fn snapshot_response(
    query: String,
    queries: Arc<BTreeSet<String>>,
    snapshots: Option<NativeQuerySnapshots>,
    stopped: CancellationToken,
) -> axum::response::Response {
    let response = async {
        if !queries.contains(&query) {
            return StatusCode::NOT_FOUND.into_response();
        }
        let Some(snapshots) = snapshots else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Query snapshot service is not bound",
            )
                .into_response();
        };
        let snapshot = tokio::select! {
            biased;
            _ = stopped.cancelled() => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            snapshot = snapshots.snapshot(&query) => match snapshot {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    eprintln!("drasi.network SSE snapshot for {query} failed: {error:#}");
                    let status = if error
                        .downcast_ref::<drasi_computation_plugin_sdk::transport::Failure>()
                        .is_some_and(|failure| failure.code == drasi_computation_plugin_sdk::abi::status::BUSY)
                    {
                        StatusCode::TOO_MANY_REQUESTS
                    } else {
                        StatusCode::SERVICE_UNAVAILABLE
                    };
                    return (status, "Query snapshot is unavailable").into_response();
                }
            },
        };
        let sequence = snapshot.metadata.as_of_sequence;
        let generation = snapshot.metadata.generation;
        let mut response =
            axum::body::Body::from_stream(snapshot_body(snapshot, query, stopped)).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            "application/json".parse().expect("constant header"),
        );
        response.headers_mut().insert(
            "x-drasi-query-sequence",
            sequence.into(),
        );
        response.headers_mut().insert(
            "x-drasi-query-generation",
            generation.into(),
        );
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
        "x-drasi-query-sequence,x-drasi-query-generation"
            .parse()
            .expect("constant header"),
    );
    response
}

fn snapshot_body(
    snapshot: NativeQuerySnapshot,
    query: String,
    stopped: CancellationToken,
) -> impl futures_util::Stream<Item = std::result::Result<String, std::io::Error>> + Send {
    let mut rows = snapshot.rows;
    async_stream::try_stream! {
        use futures_util::StreamExt;
        yield String::from("[");
        let mut first = true;
        loop {
            let row = tokio::select! {
                biased;
                _ = stopped.cancelled() => Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "SSE snapshot stopped",
                )),
                row = rows.next() => Ok(row),
            }?;
            let Some(row) = row else { break; };
            let row = row.map_err(|error| {
                eprintln!("drasi.network SSE snapshot stream for {query} failed: {error:#}");
                std::io::Error::other("SSE query snapshot stream failed")
            })?;
            let encoded = row.to_string();
            yield if first {
                first = false;
                encoded
            } else {
                format!(",{encoded}")
            };
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
        let snapshot_queries = Arc::new(
            self.config
                .query_streams
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
        );
        let snapshot_stop = cancel.clone();
        let router = Router::new()
            .route(
                "/snapshot/:query_id",
                get(
                    move |axum::extract::Path(query): axum::extract::Path<String>| {
                        snapshot_response(
                            query,
                            snapshot_queries.clone(),
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
        let result = query_result(&input, &self.config.query_streams)?;
        let events =
            self.renderer
                .render(&result, chrono::Utc::now().timestamp_millis(), |message| {
                    eprintln!("drasi.network SSE {}: {message}", self.descriptor.id())
                });
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

pub fn sse_payload(input: &InputEnvelope, queries: &BTreeMap<String, StreamId>) -> Result<Value> {
    let result = query_result(input, queries)?;
    Ok(default_payload(
        &result,
        chrono::Utc::now().timestamp_millis(),
    ))
}

fn query_result(
    input: &InputEnvelope,
    queries: &BTreeMap<String, StreamId>,
) -> Result<drasi_computation_contracts::exchange::QueryResult> {
    ensure!(input.port.as_str() == "in", "unexpected SSE input port");
    let envelope = &input.envelope;
    ensure!(
        !QueryChangeCodec::is_snapshot(envelope) && !QueryChangeCodec::is_progress_only(envelope),
        "SSE notifications do not implement snapshot/recovery control"
    );
    let metadata = QueryChangeCodec::metadata(envelope)?;
    ensure!(
        queries.get(&metadata.query_id) == Some(envelope.system().stream()),
        "unexpected SSE query/stream"
    );
    // Reuse the established browser wire projection, not a legacy runtime adapter.
    Ok(QueryChangeCodec::to_legacy_result(envelope)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{FutureExt, StreamExt};

    fn sink() -> Result<SseSink> {
        let config = SseSinkConfig::parse(json!({
            "queryStreams": {"q": "q/out"}, "host": "127.0.0.1", "port": 0,
        }))?;
        Ok(SseSink {
            descriptor: SseFactory
                .metadata()
                .descriptor(ComponentId::try_new("ui")?)?,
            renderer: config.renderer(),
            snapshots: None,
            config,
            running: None,
        })
    }

    #[test]
    fn configuration_rejects_invalid_and_unsupported_options() {
        for value in [
            json!({"queryStreams": {}}),
            json!({"queryStreams": {"q": "q/out"}, "heartbeatIntervalMs": 0}),
            json!({"queryStreams": {"q": "q/out"}, "broadcastCapacity": 0}),
            json!({"queryStreams": {"q": "q/out"}, "ssePath": "events"}),
            json!({"queryStreams": {"q": "q/out"}, "maxPaths": 0}),
            json!({"queryStreams": {"q": "q/out"}, "routes": {"missing": {}}}),
            json!({"queryStreams": {"q": "q/out"}, "routes": {"q": {"added": {"templat": "typo"}}}}),
        ] {
            assert!(SseSinkConfig::parse(value).is_err());
        }
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
