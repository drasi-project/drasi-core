// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Result;
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::Html,
    routing::{get, post},
    Json, Router,
};
use drasi_lib::observability::{
    ComponentLogKey, ComponentLogRuntime, ComponentLogType, NativeSpanCollector,
};
use metrics_exporter_prometheus::PrometheusHandle;
use serde_json::{json, Value};
use std::{future::IntoFuture, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

#[derive(Clone)]
pub struct DashboardState {
    pub metrics: PrometheusHandle,
    pub logging: ComponentLogRuntime,
    pub spans: NativeSpanCollector,
    pub lab: Arc<super::scenarios::Lab>,
    pub jaeger: String,
    pub postgres_command: Option<String>,
}

pub struct Dashboard {
    pub task: JoinHandle<std::io::Result<()>>,
    stop: oneshot::Sender<()>,
}

impl Dashboard {
    pub async fn start(port: u16, state: DashboardState) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let (stop, stopped) = oneshot::channel();
        let metrics = state.metrics.clone();
        let app = Router::new()
            .route("/", get(|| async { Html(include_str!("index.html")) }))
            .route("/health", get(|| async { "ready" }))
            .route("/metrics", get(metric_text))
            .route("/logs", get(logs))
            .route("/status", get(status))
            .route("/scenario", get(scenario))
            .route("/action", post(action))
            .with_state(state);
        let task = tokio::spawn(async move {
            let server = axum::serve(listener, app).with_graceful_shutdown(async {
                let _requested = stopped.await;
            });
            let server = server.into_future();
            tokio::pin!(server);
            let mut upkeep = tokio::time::interval(Duration::from_secs(2));
            loop {
                tokio::select! {
                    result = &mut server => return result,
                    _ = upkeep.tick() => metrics.run_upkeep(),
                }
            }
        });
        Ok(Self { task, stop })
    }

    pub async fn shutdown(mut self, already_joined: bool) -> Result<()> {
        let _closed = self.stop.send(());
        if already_joined {
            return Ok(());
        }
        match tokio::time::timeout(Duration::from_secs(3), &mut self.task).await {
            Ok(result) => result??,
            Err(error) => {
                self.task.abort();
                match self.task.await {
                    Ok(result) => result?,
                    Err(join) if join.is_cancelled() => {}
                    Err(join) => return Err(join.into()),
                }
                return Err(error.into());
            }
        }
        Ok(())
    }
}

async fn metric_text(
    State(state): State<DashboardState>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    state.metrics.run_upkeep();
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(),
    )
}

async fn logs(State(state): State<DashboardState>) -> Json<Vec<Value>> {
    let mut logs = Vec::new();
    let source = state.lab.config.source();
    let mut components = vec![
        (ComponentLogType::Source, source.to_owned()),
        (ComponentLogType::Query, format!("{source}-query")),
    ];
    if state.lab.config.scenario == "latency" {
        components.extend([
            (ComponentLogType::Sink, "http-fast".into()),
            (ComponentLogType::Sink, "http-slow".into()),
        ]);
    } else {
        components.push((ComponentLogType::Sink, format!("{source}-output")));
    }
    for (kind, component) in components {
        let key = ComponentLogKey::new(super::INSTANCE, kind, component);
        for message in state.logging.registry().get_history_by_key(&key).await {
            let trace = message
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.context.as_ref());
            logs.push(json!({
                "timestamp": message.timestamp,
                "level": message.level.to_string(),
                "component": message.component_id,
                "message": message.message,
                "target": message.metadata.as_ref().map(|metadata| &metadata.target),
                "attributes": message.metadata.as_ref().map(|metadata| &metadata.attributes),
                "trace_id": trace.map(|trace| opentelemetry::trace::TraceId::from_bytes(trace.trace_id()).to_string()),
                "span_id": trace.map(|trace| opentelemetry::trace::SpanId::from_bytes(trace.span_id()).to_string()),
                "sampled": trace.map(|trace| trace.sampled()),
            }));
        }
    }
    logs.sort_by(|left, right| left["timestamp"].as_str().cmp(&right["timestamp"].as_str()));
    Json(logs)
}

async fn status(State(state): State<DashboardState>) -> Json<Value> {
    let losses = state.spans.losses();
    let retained = state.spans.retained();
    Json(json!({
        "service": super::SERVICE,
        "instance": super::INSTANCE,
        "jaeger": state.jaeger,
        "sampling": state.lab.config.sampling,
        "config": state.lab.config.json(),
        "postgres_command": state.postgres_command,
        "span_buffer": match retained {
            Ok((records, bytes)) => json!({"records": records, "bytes": bytes}),
            Err(error) => json!({"error": error.to_string()}),
        },
        "span_losses": {
            "full": losses.full, "invalid": losses.invalid, "closed": losses.closed,
            "shutdown_timeout": losses.shutdown_timeout, "exporter": losses.exporter,
        },
        "log_losses": {
            "full": state.logging.losses().full,
            "closed": state.logging.losses().closed,
        },
    }))
}

type ApiResult = std::result::Result<Json<Value>, (StatusCode, Json<Value>)>;

fn api_error(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    tracing::warn!(%error, "Observability demo action failed");
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": error.to_string()})),
    )
}

async fn scenario(State(state): State<DashboardState>) -> ApiResult {
    state.lab.snapshot().await.map(Json).map_err(api_error)
}

async fn action(State(state): State<DashboardState>, Json(body): Json<Value>) -> ApiResult {
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| api_error(anyhow::anyhow!("action must be a string")))?;
    state.lab.action(action).await.map(Json).map_err(api_error)
}
