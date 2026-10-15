// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional host endpoint: it owns a runtime delivery runner, unlike the plugin sender.

use super::{message_limit, Batch, DELIVERY_PATH};
use anyhow::Result;
use axum::{
    extract::{DefaultBodyLimit, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use drasi_computation_contracts::{DeliveryHandler, EnvelopeCodec, InputEnvelope, ReplayRejection};
use drasi_lib::computation::v1::{DeliveryError, DeliveryRunner};
use std::{num::NonZeroUsize, sync::Arc, time::Duration};
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

struct Consumer {
    runner: DeliveryRunner,
    codec: EnvelopeCodec,
    handler: Option<Box<dyn DeliveryHandler>>,
}

/// Reference endpoint with bounded admission and actual completion. Stable IDs
/// alone do not make a nontransactional handler's external effects once-only.
/// Shutdown cancels active requests and cleans the runner. Then join the caller's
/// HTTP server and destination connection driver; this object spawns no worker.
pub struct HttpDeliveryEndpoint {
    consumer: Arc<Mutex<Consumer>>,
    admission: Arc<Admission>,
    max_message_bytes: NonZeroUsize,
}

struct Admission {
    slots: Arc<Semaphore>,
    cancelled: CancellationToken,
}

impl HttpDeliveryEndpoint {
    pub fn new(
        runner: DeliveryRunner,
        codec: EnvelopeCodec,
        handler: Box<dyn DeliveryHandler>,
        max_message_bytes: NonZeroUsize,
    ) -> Result<Self> {
        message_limit(max_message_bytes)?;
        if runner.uses_transactional_state() {
            return Err(DeliveryError::ModeMismatch.into());
        }
        Ok(Self {
            consumer: Arc::new(Mutex::new(Consumer {
                runner,
                codec,
                handler: Some(handler),
            })),
            admission: Arc::new(Admission {
                slots: Arc::new(Semaphore::new(1)),
                cancelled: CancellationToken::new(),
            }),
            max_message_bytes,
        })
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route(DELIVERY_PATH, post(deliver))
            .with_state(self.consumer.clone())
            .layer(DefaultBodyLimit::max(self.max_message_bytes.get()))
            .layer(middleware::from_fn_with_state(
                self.admission.clone(),
                admit,
            ))
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.admission.slots.close();
        self.admission.cancelled.cancel();
        let mut consumer = self.consumer.lock().await;
        consumer.runner.shutdown().await?;
        consumer.handler.take();
        Ok(())
    }
}

fn failure(status: StatusCode, error: anyhow::Error) -> Response {
    eprintln!("drasi.network HTTP delivery failed: {error:#}");
    (
        status,
        Json(serde_json::json!({"error": error.to_string()})),
    )
        .into_response()
}

async fn admit(State(admission): State<Arc<Admission>>, request: Request, next: Next) -> Response {
    let _permit = match admission.slots.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(error) => {
            let status = if error == tokio::sync::TryAcquireError::Closed {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::TOO_MANY_REQUESTS
            };
            return failure(status, error.into());
        }
    };
    tokio::select! {
        biased;
        _ = admission.cancelled.cancelled() => failure(StatusCode::SERVICE_UNAVAILABLE, anyhow::anyhow!("delivery endpoint is stopping")),
        result = tokio::time::timeout(Duration::from_secs(60), next.run(request)) => match result {
            Ok(response) => response,
            Err(error) => failure(StatusCode::GATEWAY_TIMEOUT, error.into()),
        },
    }
}

async fn deliver(
    State(consumer): State<Arc<Mutex<Consumer>>>,
    Json(batch): Json<Batch>,
) -> Response {
    let mut consumer = consumer.lock().await;
    let Consumer {
        runner,
        codec,
        handler,
    } = &mut *consumer;
    let Some(handler) = handler.as_mut() else {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            anyhow::anyhow!("delivery endpoint is stopped"),
        );
    };
    if batch.version != 1 {
        return failure(
            StatusCode::BAD_REQUEST,
            anyhow::anyhow!("unsupported HTTP delivery version"),
        );
    }
    let envelope = match codec.decode(batch.envelope.get().as_bytes()) {
        Ok(envelope) => envelope,
        Err(error) => return failure(StatusCode::BAD_REQUEST, error.into()),
    };
    let input = InputEnvelope {
        port: batch.port,
        envelope,
    };
    let identity = match runner.batch_identity(&input) {
        Ok(identity) => identity,
        Err(error) => return failure(StatusCode::BAD_REQUEST, error),
    };
    if identity != batch.identity {
        return failure(
            StatusCode::CONFLICT,
            anyhow::anyhow!("delivery identity does not match actual content and consumer scope"),
        );
    }
    match runner.deliver(&input, handler.as_mut()).await {
        Ok(()) => Json(identity).into_response(),
        Err(error) => {
            let status = match error.downcast_ref::<DeliveryError>() {
                Some(DeliveryError::Handling { source, .. }) if !handler.retryable(source) => {
                    StatusCode::UNPROCESSABLE_ENTITY
                }
                Some(
                    DeliveryError::Pending { .. }
                    | DeliveryError::StreamCapacity
                    | DeliveryError::ScopeMismatch
                    | DeliveryError::ModeMismatch,
                ) => StatusCode::CONFLICT,
                _ if error.downcast_ref::<ReplayRejection>().is_some() => StatusCode::CONFLICT,
                _ => StatusCode::SERVICE_UNAVAILABLE,
            };
            failure(status, error)
        }
    }
}
