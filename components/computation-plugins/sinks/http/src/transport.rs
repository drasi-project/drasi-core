// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! HTTP client construction, delivery with retries, and failure
//! classification shared by the sink and the delivery consumer.
//!
//! Classification uses only the response status and the `reqwest` error kind,
//! never error text. A retry loop keeps all state in its future: the host
//! cancels an in-flight `handle` by dropping it, which also cancels any
//! backoff sleep.

use anyhow::Result;
use bytes::Bytes;
use drasi_computation_network_common::{
    config::{sanitize, ClientTls, DELIVERY_ID_HEADER},
    diag,
    retry::{parse_retry_after, Retry, RetryPolicy},
};
use drasi_http_common::reaction::process::is_retryable_status;
use reqwest::{header::HeaderMap, Method, StatusCode};
use std::time::Duration;

/// Largest response body read for diagnostics or to reuse the connection.
pub const MAX_RESPONSE_BODY_BYTES: usize = 4096;

/// Connection settings fixed for the life of one client (one activation).
pub(crate) struct ClientSettings<'a> {
    pub connect_timeout: Duration,
    pub pool_idle_timeout: Duration,
    pub pool_max_idle_per_host: usize,
    pub tls: &'a ClientTls,
}

/// Builds a rustls client that never follows redirects. Per-attempt timeouts
/// are set on each request so they can change in place.
pub(crate) fn build_client(settings: &ClientSettings<'_>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(settings.connect_timeout)
        .pool_idle_timeout(settings.pool_idle_timeout)
        .pool_max_idle_per_host(settings.pool_max_idle_per_host);
    if let Some(ca) = &settings.tls.ca_pem {
        for certificate in reqwest::Certificate::from_pem_bundle(ca.as_bytes())? {
            builder = builder.add_root_certificate(certificate);
        }
    }
    if let (Some(certificate), Some(key)) =
        (&settings.tls.certificate_pem, &settings.tls.private_key_pem)
    {
        let mut pem = Vec::with_capacity(certificate.len() + key.expose().len() + 1);
        pem.extend_from_slice(certificate.as_bytes());
        pem.push(b'\n');
        pem.extend_from_slice(key.expose().as_bytes());
        builder = builder.identity(reqwest::Identity::from_pem(&pem)?);
    }
    Ok(builder.build()?)
}

/// A destination's response that did not complete the delivery.
#[derive(Debug, thiserror::Error)]
#[error("HTTP destination returned status {status}{}", if body.is_empty() { String::new() } else { format!(": {body}") })]
pub struct StatusError {
    pub status: StatusCode,
    /// The first bytes of the response body, sanitized for logs.
    pub body: String,
    pub retryable: bool,
}

/// Whether a transport failure may succeed on retry: connection failures,
/// timeouts and failures while the request was in flight (whose outcome is
/// unknown; the retry carries the same delivery identity).
pub(crate) fn transport_retryable(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout() || error.is_request()
}

/// Whether an error returned by [`deliver`] is worth retrying.
pub fn retryable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<StatusError>()
        .is_some_and(|error| error.retryable)
        || error
            .downcast_ref::<reqwest::Error>()
            .is_some_and(transport_retryable)
}

/// One fully rendered request.
pub(crate) struct Request {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// Per-delivery settings that can change in place.
pub(crate) struct Attempts<'a> {
    pub timeout: Duration,
    pub retry: RetryPolicy,
    pub success: &'a [u16],
}

/// Reads at most `cap` bytes of a response body, ignoring read failures.
pub(crate) async fn read_capped(mut response: reqwest::Response, cap: usize) -> Vec<u8> {
    let mut body = Vec::new();
    while body.len() < cap {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let take = chunk.len().min(cap - body.len());
                body.extend_from_slice(&chunk[..take]);
            }
            _ => break,
        }
    }
    body
}

/// Sends `request` until a configured success status, a permanent failure,
/// or the attempts or budget run out. Every attempt carries the same
/// `x-drasi-delivery-id`.
pub(crate) async fn deliver(
    client: &reqwest::Client,
    request: &Request,
    attempts: &Attempts<'_>,
    delivery_id: &str,
    label: &str,
) -> Result<()> {
    let mut retry = Retry::new(attempts.retry);
    loop {
        let sent = client
            .request(request.method.clone(), &request.url)
            .headers(request.headers.clone())
            .header(DELIVERY_ID_HEADER, delivery_id)
            .timeout(attempts.timeout)
            .body(request.body.clone())
            .send()
            .await;
        let (error, requested) = match sent {
            Ok(response) => {
                let status = response.status();
                let requested = matches!(
                    status,
                    StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
                )
                .then(|| {
                    response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(parse_retry_after)
                })
                .flatten();
                let body = read_capped(response, MAX_RESPONSE_BODY_BYTES).await;
                if attempts.success.contains(&status.as_u16()) {
                    return Ok(());
                }
                let error = StatusError {
                    status,
                    body: sanitize(&String::from_utf8_lossy(&body)),
                    retryable: is_retryable_status(status),
                };
                (anyhow::Error::from(error), requested)
            }
            Err(error) => (anyhow::Error::from(error.without_url()), None),
        };
        if !retryable(&error) {
            return Err(error.context("permanent HTTP delivery failure"));
        }
        let Some(delay) = retry.next_delay(requested) else {
            return Err(error.context(format!(
                "HTTP delivery failed after {} attempts",
                retry.attempt() + 1
            )));
        };
        diag!(
            Warn,
            "HTTP delivery retry: output={} delivery={} attempt={} delay_ms={} error={}",
            sanitize(label),
            sanitize(delivery_id),
            retry.attempt(),
            delay.as_millis(),
            sanitize(&format!("{error:#}"))
        );
        tokio::time::sleep(delay).await;
    }
}
