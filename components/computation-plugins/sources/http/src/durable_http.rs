// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The HTTP producer-session protocol under `/sources/{id}/admission/v1`.
//!
//! Clients register a producer, then submit events with consecutive
//! sequences. Retrying the same session and sequence is idempotent. These
//! routes require the shared bearer token when one is configured.

use crate::{
    component::HttpIngress,
    durable::{invalid, BatchOutcome},
    wire::{self, HttpSourceChange},
};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use drasi_computation_contracts::{ComponentId, ProducerSession};
use drasi_computation_network_common::diag;
use drasi_computation_plugin_sdk::{AdmissionError, AdmissionErrorKind};
use serde::{de::DeserializeOwned, Deserialize};
use std::sync::Arc;

fn status(error: &AdmissionError) -> StatusCode {
    use AdmissionErrorKind::*;
    match error.kind {
        Busy | CapacityExhausted => StatusCode::TOO_MANY_REQUESTS,
        Closed | AcceptanceUnknown | MetadataUnknown => StatusCode::SERVICE_UNAVAILABLE,
        NotEnabled | Sequence | PayloadConflict | PendingObligations => StatusCode::CONFLICT,
        SessionExpired | ReceiptExpired => StatusCode::GONE,
        IdentifierTooLong | Invalid => StatusCode::BAD_REQUEST,
    }
}

fn error_response(error: AdmissionError) -> Response {
    diag!(Debug, "durable HTTP request failed: {error}");
    (status(&error), Json(serde_json::json!({ "error": error }))).into_response()
}

fn response<T: serde::Serialize>(result: Result<T, AdmissionError>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => error_response(error),
    }
}

fn parse<T: DeserializeOwned>(body: &Bytes) -> Result<T, AdmissionError> {
    serde_json::from_slice(body).map_err(|error| invalid(format!("invalid JSON body: {error}")))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Register {
    producer: ComponentId,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    session: ProducerSession,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    session: ProducerSession,
    sequence: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Event {
    session: ProducerSession,
    sequence: u64,
    event: HttpSourceChange,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    session: ProducerSession,
    first_sequence: u64,
    events: Vec<HttpSourceChange>,
}

async fn register(
    State(ingress): State<Arc<HttpIngress>>,
    Path(source): Path<String>,
    body: Bytes,
) -> Response {
    let shared = &ingress.shared;
    response(
        async {
            shared.check_source(&source)?;
            let request: Register = parse(&body)?;
            if request.producer == shared.source_id {
                return Err(invalid(
                    "the source's own component UUID is reserved for its server-sequenced producer",
                ));
            }
            shared
                .admission_call(
                    AdmissionErrorKind::MetadataUnknown,
                    shared
                        .admission_service()?
                        .register_producer(&request.producer),
                )
                .await
        }
        .await,
    )
}

async fn retire(
    State(ingress): State<Arc<HttpIngress>>,
    Path(source): Path<String>,
    body: Bytes,
) -> Response {
    let shared = &ingress.shared;
    response(
        async {
            shared.check_source(&source)?;
            let request: Session = parse(&body)?;
            shared
                .admission_call(
                    AdmissionErrorKind::MetadataUnknown,
                    shared
                        .admission_service()?
                        .retire_producer(&request.session),
                )
                .await?;
            Ok(serde_json::json!({ "retired": true }))
        }
        .await,
    )
}

async fn producer_status(
    State(ingress): State<Arc<HttpIngress>>,
    Path(source): Path<String>,
    body: Bytes,
) -> Response {
    let shared = &ingress.shared;
    response(
        async {
            shared.check_source(&source)?;
            let request: Session = parse(&body)?;
            shared
                .admission_call(
                    AdmissionErrorKind::Closed,
                    shared
                        .admission_service()?
                        .producer_status(&request.session),
                )
                .await
        }
        .await,
    )
}

async fn receipt(
    State(ingress): State<Arc<HttpIngress>>,
    Path(source): Path<String>,
    body: Bytes,
) -> Response {
    let shared = &ingress.shared;
    response(
        async {
            shared.check_source(&source)?;
            let request: Receipt = parse(&body)?;
            shared
                .admission_call(
                    AdmissionErrorKind::Closed,
                    shared
                        .admission_service()?
                        .admission_receipt(&request.session, request.sequence),
                )
                .await
        }
        .await,
    )
}

async fn event(
    State(ingress): State<Arc<HttpIngress>>,
    Path(source): Path<String>,
    body: Bytes,
) -> Response {
    match parse::<Event>(&body) {
        Ok(request) => {
            submit(
                &ingress,
                &source,
                request.session,
                request.sequence,
                vec![request.event],
            )
            .await
        }
        Err(error) => admission_response(Err(error)),
    }
}

async fn batch(
    State(ingress): State<Arc<HttpIngress>>,
    Path(source): Path<String>,
    body: Bytes,
) -> Response {
    match parse::<Batch>(&body) {
        Ok(request) => {
            submit(
                &ingress,
                &source,
                request.session,
                request.first_sequence,
                request.events,
            )
            .await
        }
        Err(error) => admission_response(Err(error)),
    }
}

async fn submit(
    ingress: &HttpIngress,
    source: &str,
    session: ProducerSession,
    sequence: u64,
    events: Vec<HttpSourceChange>,
) -> Response {
    let shared = &ingress.shared;
    let result = async {
        shared.check_source(source)?;
        if events.is_empty() || events.len() > ingress.settings().max_batch_events {
            return Err(invalid("batch is empty or exceeds maxBatchEvents"));
        }
        let changes = events
            .into_iter()
            .map(|event| {
                let timestamp = match &event {
                    HttpSourceChange::Insert { timestamp, .. }
                    | HttpSourceChange::Update { timestamp, .. }
                    | HttpSourceChange::Delete { timestamp, .. } => timestamp,
                };
                if timestamp.is_none() {
                    return Err(invalid(
                        "session-protocol events require an explicit timestamp",
                    ));
                }
                wire::http_change(event, &shared.source_id).map_err(invalid)
            })
            .collect::<Result<Vec<_>, _>>()?;
        shared.admit_durable(&session, sequence, changes).await
    }
    .await;
    admission_response(result)
}

pub(crate) fn admission_response(result: Result<BatchOutcome, AdmissionError>) -> Response {
    match result {
        Ok(outcome) => {
            let code = outcome.error.as_ref().map_or(StatusCode::OK, |error| {
                diag!(
                    Debug,
                    "durable HTTP admission stopped after {} receipts: {error}",
                    outcome.receipts.len()
                );
                status(error)
            });
            (code, Json(outcome)).into_response()
        }
        Err(error) => {
            diag!(Debug, "durable HTTP admission rejected: {error}");
            (
                status(&error),
                Json(BatchOutcome {
                    receipts: Vec::new(),
                    error: Some(error),
                }),
            )
                .into_response()
        }
    }
}

pub(crate) fn routes() -> Router<Arc<HttpIngress>> {
    Router::new()
        .route(
            "/sources/:source/admission/v1/producers/register",
            post(register),
        )
        .route(
            "/sources/:source/admission/v1/producers/status",
            post(producer_status),
        )
        .route(
            "/sources/:source/admission/v1/producers/retire",
            post(retire),
        )
        .route("/sources/:source/admission/v1/receipts", post(receipt))
        .route("/sources/:source/admission/v1/events", post(event))
        .route("/sources/:source/admission/v1/events/batch", post(batch))
}
