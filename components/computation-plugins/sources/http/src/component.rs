// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_plugin_sdk::{NativeComponent, NativeSource};

use crate::{
    config::HttpSourceConfig,
    ingress::{Ingress as SharedIngress, SourceRuntime},
    wire::{self, HttpSourceChange},
};
use anyhow::Result;
use async_trait::async_trait;
use axum::{
    extract::{DefaultBodyLimit, Path, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use drasi_computation_contracts::{ComponentDescriptor, OutputEnvelope};
use drasi_computation_plugin_sdk::{NativeAdmission, Scope};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::net::TcpListener;

pub(crate) struct HttpSource {
    runtime: SourceRuntime,
    config: HttpSourceConfig,
}
impl HttpSource {
    pub fn new(
        descriptor: ComponentDescriptor,
        config: HttpSourceConfig,
        scope: Option<&Scope>,
        admission: Option<NativeAdmission>,
    ) -> Result<Self> {
        Ok(Self {
            runtime: SourceRuntime::new(descriptor, config.ingress(), scope, admission)?,
            config,
        })
    }
}
#[async_trait]
impl NativeComponent for HttpSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.runtime.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.config)?)
    }
    async fn start(&mut self) -> Result<()> {
        let max_batch_events = self
            .config
            .max_batch_events
            .expect("validated HTTP batch limit");
        self.runtime
            .start(move |listener, ingress| {
                serve(
                    listener,
                    Arc::new(Ingress {
                        shared: ingress,
                        max_batch_events,
                    }),
                )
            })
            .await
    }
    async fn stop(&mut self) -> Result<()> {
        self.runtime.stop().await
    }
}
#[async_trait]
impl NativeSource for HttpSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        self.runtime.next().await
    }
}

pub(crate) struct Ingress {
    shared: Arc<SharedIngress>,
    pub max_batch_events: usize,
}

impl std::ops::Deref for Ingress {
    type Target = SharedIngress;

    fn deref(&self) -> &Self::Target {
        &self.shared
    }
}

#[derive(Serialize)]
struct EventResponse {
    success: bool,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    events_processed: usize,
}
fn failure(status: StatusCode, error: impl ToString, accepted: usize) -> Response {
    let error = error.to_string();
    eprintln!("drasi.network HTTP admission failed: accepted={accepted} error={error}");
    (
        status,
        Json(EventResponse {
            success: false,
            message: format!("Admission failed after {accepted} events"),
            error: Some(error),
            events_processed: accepted,
        }),
    )
        .into_response()
}
async fn submit(
    State(ingress): State<Arc<Ingress>>,
    Path(source): Path<String>,
    Json(event): Json<HttpSourceChange>,
) -> Response {
    submit_events(&ingress, source, vec![event]).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    events: Vec<HttpSourceChange>,
}
async fn batch(
    State(ingress): State<Arc<Ingress>>,
    Path(source): Path<String>,
    Json(batch): Json<Batch>,
) -> Response {
    if batch.events.is_empty() || batch.events.len() > ingress.max_batch_events {
        return failure(
            StatusCode::BAD_REQUEST,
            "batch count exceeds configured maxBatchEvents or is empty",
            0,
        );
    }
    submit_events(&ingress, source, batch.events).await
}
async fn submit_events(
    ingress: &Ingress,
    source: String,
    events: Vec<HttpSourceChange>,
) -> Response {
    if ingress.durable().is_some() {
        return failure(
            StatusCode::CONFLICT,
            "durable source requires /admission/v1 endpoints",
            0,
        );
    }
    if source != ingress.config.source_id {
        return failure(StatusCode::BAD_REQUEST, "source ID mismatch", 0);
    }
    let events = match events
        .into_iter()
        .map(|event| wire::http_change(event, &source))
        .collect::<Result<Vec<_>>>()
    {
        Ok(events) => events,
        Err(error) => return failure(StatusCode::BAD_REQUEST, error, 0),
    };
    match ingress.admit(events).await {
        Ok(accepted) => Json(EventResponse {
            success: true,
            message: format!("All {accepted} events processed successfully"),
            error: None,
            events_processed: accepted,
        })
        .into_response(),
        Err(error) => failure(StatusCode::SERVICE_UNAVAILABLE, error.error, error.accepted),
    }
}
async fn health(State(ingress): State<Arc<Ingress>>) -> Response {
    if ingress.cancel.is_cancelled() {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "source stopped", 0);
    }
    Json(
        serde_json::json!({"status":"healthy","service":"http-source","features":["batch-endpoint"],
        "volatile":ingress.durable().is_none()}),
    )
    .into_response()
}
async fn request_scope(
    State(ingress): State<Arc<Ingress>>,
    request: Request,
    next: Next,
) -> Response {
    let _permit = if request.uri().path() == "/health" {
        None
    } else {
        match ingress.request_permit() {
            Ok(permit) => Some(permit),
            Err(error) => return failure(StatusCode::SERVICE_UNAVAILABLE, error, 0),
        }
    };
    // Admission owns its own deadline and reports partial accepted counts. This
    // outer deadline additionally bounds a client stalled while sending a body.
    // Do not race a longer body+admission deadline against an in-progress batch.
    let body_limit = ingress.config.max_message_bytes;
    let (parts, body) = request.into_parts();
    let bytes = tokio::select! {
        biased;
        _ = ingress.cancel.cancelled() => return failure(StatusCode::SERVICE_UNAVAILABLE, "source stopped", 0),
        bytes = tokio::time::timeout(std::time::Duration::from_millis(ingress.config.timeout_ms),
            axum::body::to_bytes(body, body_limit)) => match bytes {
                Ok(Ok(bytes)) => bytes,
                Ok(Err(error)) => return failure(StatusCode::PAYLOAD_TOO_LARGE, error, 0),
                Err(_) => return failure(StatusCode::REQUEST_TIMEOUT, "request body timeout", 0),
            },
    };
    next.run(Request::from_parts(parts, axum::body::Body::from(bytes)))
        .await
}
async fn serve(listener: TcpListener, ingress: Arc<Ingress>) -> Result<()> {
    let cancel = ingress.cancel.clone();
    let routes = Router::new()
        .route("/sources/:source/events", post(submit))
        .route("/sources/:source/events/batch", post(batch))
        .route("/health", get(health))
        .merge(crate::durable_http::routes())
        .layer(DefaultBodyLimit::max(ingress.config.max_message_bytes))
        .layer(middleware::from_fn_with_state(
            ingress.clone(),
            request_scope,
        ))
        .with_state(ingress);
    axum::serve(listener, routes)
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await?;
    Ok(())
}
