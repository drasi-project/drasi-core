// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{
    durable::invalid,
    ingress::Ingress,
    wire::{self, HttpSourceChange},
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use drasi_computation_plugin_sdk::{AdmissionError, AdmissionErrorKind};
use drasi_lib::computation::v1::{ComponentId, ProducerSession};
use serde::Deserialize;
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
    eprintln!("drasi.network durable HTTP request failed: {error}");
    (status(&error), Json(serde_json::json!({"error":error}))).into_response()
}
fn response<T: serde::Serialize>(result: Result<T, AdmissionError>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => error_response(error),
    }
}
fn check(ingress: &Ingress, source: &str) -> Result<(), AdmissionError> {
    if source != ingress.config.source_id {
        return Err(invalid("source ID mismatch"));
    }
    ingress.admission_service()?;
    Ok(())
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
    State(ingress): State<Arc<Ingress>>,
    Path(source): Path<String>,
    Json(request): Json<Register>,
) -> Response {
    response(
        async {
            check(&ingress, &source)?;
            ingress
                .admission_call(
                    AdmissionErrorKind::MetadataUnknown,
                    ingress
                        .admission_service()?
                        .register_producer(&request.producer),
                )
                .await
        }
        .await,
    )
}
async fn retire(
    State(ingress): State<Arc<Ingress>>,
    Path(source): Path<String>,
    Json(request): Json<Session>,
) -> Response {
    response(
        async {
            check(&ingress, &source)?;
            ingress
                .admission_call(
                    AdmissionErrorKind::MetadataUnknown,
                    ingress
                        .admission_service()?
                        .retire_producer(&request.session),
                )
                .await?;
            Ok(serde_json::json!({"retired":true}))
        }
        .await,
    )
}
async fn producer_status(
    State(ingress): State<Arc<Ingress>>,
    Path(source): Path<String>,
    Json(request): Json<Session>,
) -> Response {
    response(
        async {
            check(&ingress, &source)?;
            ingress
                .admission_call(
                    AdmissionErrorKind::Closed,
                    ingress
                        .admission_service()?
                        .producer_status(&request.session),
                )
                .await
        }
        .await,
    )
}
async fn receipt(
    State(ingress): State<Arc<Ingress>>,
    Path(source): Path<String>,
    Json(request): Json<Receipt>,
) -> Response {
    response(
        async {
            check(&ingress, &source)?;
            ingress
                .admission_call(
                    AdmissionErrorKind::Closed,
                    ingress
                        .admission_service()?
                        .admission_receipt(&request.session, request.sequence),
                )
                .await
        }
        .await,
    )
}
async fn event(
    State(ingress): State<Arc<Ingress>>,
    Path(source): Path<String>,
    Json(request): Json<Event>,
) -> Response {
    submit(
        &ingress,
        &source,
        request.session,
        request.sequence,
        vec![request.event],
    )
    .await
}
async fn batch(
    State(ingress): State<Arc<Ingress>>,
    Path(source): Path<String>,
    Json(request): Json<Batch>,
) -> Response {
    submit(
        &ingress,
        &source,
        request.session,
        request.first_sequence,
        request.events,
    )
    .await
}
async fn submit(
    ingress: &Ingress,
    source: &str,
    session: ProducerSession,
    sequence: u64,
    events: Vec<HttpSourceChange>,
) -> Response {
    let result = async {
        check(ingress, source)?;
        if events.is_empty() || events.len() > ingress.config.max_batch_events.expect("HTTP config")
        {
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
                    return Err(invalid("durable HTTP events require an explicit timestamp"));
                }
                wire::http_change(event, source).map_err(invalid)
            })
            .collect::<Result<Vec<_>, _>>()?;
        ingress.admit_durable(&session, sequence, changes).await
    }
    .await;
    match result {
        Ok(outcome) => {
            let code = outcome.error.as_ref().map_or(StatusCode::OK, |error| {
                eprintln!(
                    "drasi.network durable HTTP admission stopped after {} receipts: {error}",
                    outcome.receipts.len()
                );
                status(error)
            });
            (code, Json(outcome)).into_response()
        }
        Err(error) => {
            eprintln!("drasi.network durable HTTP admission rejected: {error}");
            (
                status(&error),
                Json(crate::durable::BatchOutcome {
                    receipts: Vec::new(),
                    error: Some(error),
                }),
            )
                .into_response()
        }
    }
}
pub(crate) fn routes() -> Router<Arc<Ingress>> {
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
