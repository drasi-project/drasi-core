// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! OTLP/HTTP with binary protobuf bodies: `POST /v1/metrics`, `/v1/traces`
//! and `/v1/logs`.
//!
//! Each request is checked in this order: rate limit and bearer token (401 or
//! 429), a request slot (503), `Content-Type: application/x-protobuf` (415;
//! OTLP/JSON is not supported), the body read within `timeoutMs` and
//! `maxMessageBytes` (408 or 413), optional `Content-Encoding: gzip`
//! decompressed within `maxMessageBytes` (413), and protobuf decoding (400).
//! A successful export answers 200 with the protobuf `Export*ServiceResponse`,
//! including partial success. Every error answers a protobuf `google.rpc.Status`
//! body, as OTLP/HTTP requires; 429 and 503 are the retryable statuses and
//! carry `Retry-After`.

use crate::engine::{Engine, ExportOutcome};
use crate::otlp::proto::collector::logs::v1::{
    ExportLogsPartialSuccess, ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use crate::otlp::proto::collector::metrics::v1::{
    ExportMetricsPartialSuccess, ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use crate::otlp::proto::collector::trace::v1::{
    ExportTracePartialSuccess, ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use crate::otlp::RpcStatus;
use crate::projection::{map_logs, map_metrics, map_traces, MapOutcome};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use bytes::Bytes;
use drasi_computation_network_common::diag;
use drasi_computation_network_common::ingress::{Ingress, IngressError, IngressErrorKind};
use drasi_computation_network_common::server::PeerAddress;
use prost::Message;
use std::io::Read;
use std::sync::Arc;

/// The OTLP/HTTP binary protobuf media type.
pub const PROTOBUF: &str = "application/x-protobuf";

#[derive(Clone)]
struct Http {
    ingress: Arc<Ingress>,
    engine: Arc<Engine>,
}

/// The OTLP/HTTP router for one activation.
pub(crate) fn router(ingress: Arc<Ingress>, engine: Arc<Engine>) -> Router {
    Router::new()
        .route("/v1/metrics", post(metrics))
        .route("/v1/traces", post(traces))
        .route("/v1/logs", post(logs))
        .fallback(|| async { error(StatusCode::NOT_FOUND, 5, "no OTLP signal at this path") })
        .with_state(Http { ingress, engine })
}

/// A protobuf `google.rpc.Status` error response. `code` is a `google.rpc.Code`.
fn error(status: StatusCode, code: i32, message: &str) -> Response {
    let body = RpcStatus {
        code,
        message: message.into(),
    }
    .encode_to_vec();
    let mut response = (status, body).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(PROTOBUF));
    if matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
    ) {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    response
}

/// The HTTP status and `google.rpc.Code` for an ingress refusal.
pub(crate) fn rejection(failure: &IngressError) -> Response {
    use IngressErrorKind::*;
    let (status, code) = match failure.kind {
        Stopped | Paused | Overloaded | Uncertain | Timeout => {
            (StatusCode::SERVICE_UNAVAILABLE, 14)
        }
        RateLimited => (StatusCode::TOO_MANY_REQUESTS, 8),
        Unauthorized => (StatusCode::UNAUTHORIZED, 16),
        Invalid => (StatusCode::BAD_REQUEST, 3),
        Failed => (StatusCode::INTERNAL_SERVER_ERROR, 13),
    };
    error(status, code, &failure.message)
}

fn ok(body: Vec<u8>) -> Response {
    let mut response = (StatusCode::OK, body).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(PROTOBUF));
    response
}

fn header_text(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// A complete error response, boxed to keep `Result`s small.
type Rejection = Box<Response>;

/// Reads, decompresses and decodes the body, after access control. `Err` is
/// the complete error response.
async fn decode<T: Message + Default>(http: &Http, request: Request) -> Result<T, Rejection> {
    let peer = request
        .extensions()
        .get::<PeerAddress>()
        .map(|peer| peer.0.ip());
    let (parts, body) = request.into_parts();
    let ingress = &http.ingress;
    ingress
        .authorize(
            peer,
            header_text(&parts.headers, header::AUTHORIZATION),
            true,
        )
        .map_err(|failure| Box::new(rejection(&failure)))?;
    let content_type = header_text(&parts.headers, header::CONTENT_TYPE).unwrap_or("");
    let media_type = content_type.split(';').next().unwrap_or("").trim();
    if !media_type.eq_ignore_ascii_case(PROTOBUF) {
        return Err(Box::new(error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            3,
            "Content-Type must be application/x-protobuf; OTLP/JSON is not supported",
        )));
    }
    let encoding = header_text(&parts.headers, header::CONTENT_ENCODING)
        .unwrap_or("identity")
        .trim()
        .to_ascii_lowercase();
    if !matches!(encoding.as_str(), "identity" | "gzip") {
        return Err(Box::new(error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            3,
            "Content-Encoding must be gzip or identity",
        )));
    }
    let live = ingress.live();
    let limit = live.max_message_bytes;
    let bytes = tokio::select! {
        biased;
        _ = ingress.shutdown.cancelled() => return Err(Box::new(rejection(&IngressError::stopped()))),
        bytes = tokio::time::timeout(live.timeout, axum::body::to_bytes(body, limit)) => match bytes {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => return Err(Box::new(error(StatusCode::PAYLOAD_TOO_LARGE, 8, "request body exceeds maxMessageBytes or could not be read"))),
            Err(_) => return Err(Box::new(error(StatusCode::REQUEST_TIMEOUT, 4, "request body was not received before timeoutMs"))),
        },
    };
    let bytes = if encoding == "gzip" {
        gunzip(&bytes, limit)?
    } else {
        bytes
    };
    T::decode(bytes).map_err(|failure| {
        diag!(
            Debug,
            "OTLP/HTTP body is not a valid export request: {failure}"
        );
        Box::new(error(
            StatusCode::BAD_REQUEST,
            3,
            "body is not a valid OTLP protobuf export request",
        ))
    })
}

/// Decompresses at most `limit` bytes; a larger result is refused rather
/// than inflated without bound.
fn gunzip(compressed: &[u8], limit: usize) -> Result<Bytes, Rejection> {
    let mut output = Vec::new();
    let mut decoder = flate2::read::GzDecoder::new(compressed).take(limit as u64 + 1);
    decoder
        .read_to_end(&mut output)
        .map_err(|_| Box::new(error(StatusCode::BAD_REQUEST, 3, "body is not valid gzip")))?;
    if output.len() > limit {
        return Err(Box::new(error(
            StatusCode::PAYLOAD_TOO_LARGE,
            8,
            "decompressed body exceeds maxMessageBytes",
        )));
    }
    Ok(output.into())
}

async fn export<T: Message + Default>(
    http: &Http,
    request: Request,
    project: impl FnOnce(&T, &crate::config::Projection, u64) -> MapOutcome,
    respond: impl FnOnce(ExportOutcome) -> Vec<u8>,
) -> Response {
    // Slot first, so an overloaded source refuses before reading a body.
    let _permit = match http.ingress.request_permit() {
        Ok(permit) => permit,
        Err(failure) => return rejection(&failure),
    };
    let message: T = match decode(http, request).await {
        Ok(message) => message,
        Err(response) => return *response,
    };
    match http
        .engine
        .ingest(&http.ingress, |settings, now| {
            project(&message, settings, now)
        })
        .await
    {
        Ok(outcome) => ok(respond(outcome)),
        Err(failure) => rejection(&failure),
    }
}

fn partial_count(outcome: ExportOutcome) -> i64 {
    i64::try_from(outcome.rejected).unwrap_or(i64::MAX)
}

async fn metrics(State(http): State<Http>, request: Request<Body>) -> Response {
    export(&http, request, map_metrics, |outcome| {
        ExportMetricsServiceResponse {
            partial_success: (outcome.rejected > 0).then(|| ExportMetricsPartialSuccess {
                rejected_data_points: partial_count(outcome),
                error_message:
                    "some data points were not projected or were dropped at a cardinality cap"
                        .into(),
            }),
        }
        .encode_to_vec()
    })
    .await
}

async fn traces(State(http): State<Http>, request: Request<Body>) -> Response {
    export(
        &http,
        request,
        |message: &ExportTraceServiceRequest, settings, now| map_traces(message, settings, now),
        |outcome| {
            ExportTraceServiceResponse {
                partial_success: (outcome.rejected > 0).then(|| ExportTracePartialSuccess {
                    rejected_spans: partial_count(outcome),
                    error_message:
                        "some spans were not projected or were dropped at a cardinality cap".into(),
                }),
            }
            .encode_to_vec()
        },
    )
    .await
}

async fn logs(State(http): State<Http>, request: Request<Body>) -> Response {
    export(
        &http,
        request,
        |message: &ExportLogsServiceRequest, settings, now| map_logs(message, settings, now),
        |outcome| {
            ExportLogsServiceResponse {
                partial_success: (outcome.rejected > 0).then(|| ExportLogsPartialSuccess {
                    rejected_log_records: partial_count(outcome),
                    error_message:
                        "some log records were not projected or were dropped at a cardinality cap"
                            .into(),
                }),
            }
            .encode_to_vec()
        },
    )
    .await
}
