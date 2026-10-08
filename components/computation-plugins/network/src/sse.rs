// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use axum::{
    http::{header, StatusCode},
    response::{sse::Event, IntoResponse, Sse},
    routing::get,
    Router,
};
use drasi_computation_plugin_sdk::{
    Capabilities, Component, ConfigField, ConfigSchema, ConfigType, ControlSender, CreateRequest,
    CreatedComponent, Factory, FactoryMetadata,
};
use drasi_lib::{
    computation::v1::*,
    context::workers::{join_owned_worker_gracefully, spawn_owned_worker, WorkerCompletion},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, convert::Infallible, net::IpAddr, time::Duration};
use tokio::{net::TcpListener, sync::broadcast, sync::RwLock, task::JoinHandle};
use tokio_util::sync::CancellationToken;

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
}
impl SseSinkConfig {
    pub fn parse(value: Value) -> Result<Self> {
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
            config.sse_path.starts_with('/')
                && !config.sse_path.contains(['?', '#'])
                && !config.sse_path.chars().any(char::is_control),
            "ssePath must be an absolute HTTP path without query or fragment"
        );
        crate::config::duration(config.heartbeat_interval_ms, "heartbeatIntervalMs")?;
        ensure!(
            (1..=1_048_576).contains(&config.broadcast_capacity),
            "broadcastCapacity must be in 1..=1048576"
        );
        Ok(config)
    }
}

pub(crate) struct SseFactory;
impl Factory for SseFactory {
    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(crate::SSE_SINK, "1")
                .expect("constant SSE implementation"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: ConfigSchema {
                fields: [
                    ("queryStreams", ConfigType::Object, true),
                    ("host", ConfigType::String, false),
                    ("port", ConfigType::Integer, false),
                    ("ssePath", ConfigType::String, false),
                    ("heartbeatIntervalMs", ConfigType::Integer, false),
                    ("broadcastCapacity", ConfigType::Integer, false),
                ]
                .into_iter()
                .map(|(name, value_type, required)| {
                    (
                        name.into(),
                        ConfigField {
                            value_type,
                            required,
                            secret: false,
                        },
                    )
                })
                .collect(),
                allow_additional: false,
            },
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
    fn create(&self, request: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        Ok(Component::Sink(Box::new(SseSink {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            config: SseSinkConfig::parse(request.configuration.clone())?,
            running: None,
        }))
        .into())
    }
}

struct Running {
    sender: broadcast::Sender<String>,
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
    running: Option<Running>,
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
impl ComputationComponent for SseSink {
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
        let (sender, _) = broadcast::channel(self.config.broadcast_capacity);
        let cancel = CancellationToken::new();
        let subscriptions = sender.clone();
        let stopped = cancel.clone();
        let path = self.config.sse_path.clone();
        let interval =
            crate::config::duration(self.config.heartbeat_interval_ms, "heartbeatIntervalMs")?;
        let router = Router::new().fallback(
            get(move |request: axum::extract::Request| {
                let receiver = subscriptions.subscribe();
                let stopped = stopped.clone();
                let matches = request.uri().path() == path;
                async move {
                    let mut response = if matches {
                        Sse::new(events(receiver, stopped, interval)).into_response()
                    } else {
                        StatusCode::NOT_FOUND.into_response()
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
            sender,
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
impl EnvelopeSink for SseSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Accepted
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        let running = self.running.as_ref().context("SSE sink has not started")?;
        ensure!(
            !running.cancel.is_cancelled(),
            "SSE listener stopped; cleanup required"
        );
        let payload = sse_payload(&input, &self.config.query_streams)?;
        // Volatile UI notifications require no listener or browser acknowledgement.
        let _ = running.sender.send(payload.to_string());
        Ok(())
    }
}

pub fn sse_payload(input: &InputEnvelope, queries: &BTreeMap<String, StreamId>) -> Result<Value> {
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
    let result = QueryChangeCodec::to_legacy_result(envelope)?;
    Ok(json!({
        "queryId": result.query_id,
        "results": result.results,
        "timestamp": chrono::Utc::now().timestamp_millis(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{FutureExt, StreamExt};

    fn sink() -> Result<SseSink> {
        Ok(SseSink {
            descriptor: SseFactory
                .metadata()
                .descriptor(ComponentId::try_new("ui")?)?,
            config: SseSinkConfig::parse(json!({
                "queryStreams": {"q": "q/out"}, "host": "127.0.0.1", "port": 0,
            }))?,
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
            json!({"queryStreams": {"q": "q/out"}, "routes": {}}),
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
            sender,
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
