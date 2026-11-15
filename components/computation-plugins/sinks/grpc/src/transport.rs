// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Channel construction and failure classification.

use crate::{
    config::{Compression, GrpcSinkConfig},
    proto::reaction,
};
use anyhow::Result;
use drasi_computation_network_common::retry::parse_retry_after;
use std::time::Duration;
use tonic::{
    codec::CompressionEncoding,
    transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity},
    Code, Status,
};

pub(crate) type ChangeClient = reaction::change_service_client::ChangeServiceClient<Channel>;

/// The tonic endpoint for a validated configuration: scheme-mapped address,
/// connect timeout, HTTP/2 keepalive and, for TLS endpoints, the bundled
/// Mozilla roots plus `tlsCaPem`, the optional client identity and server
/// name. There is no endpoint-wide request timeout: each call carries its own
/// deadline (`timeoutMs`), and connection setup has `connectTimeoutMs`.
pub(crate) fn endpoint(config: &GrpcSinkConfig) -> Result<Endpoint> {
    let destination = config.destination()?;
    let mut endpoint = Endpoint::from_shared(destination.uri)?
        .connect_timeout(Duration::from_millis(config.connect_timeout_ms))
        .tcp_nodelay(true);
    if config.keepalive_interval_ms > 0 {
        endpoint = endpoint
            .http2_keep_alive_interval(Duration::from_millis(config.keepalive_interval_ms))
            .keep_alive_timeout(Duration::from_millis(config.keepalive_timeout_ms))
            .keep_alive_while_idle(false);
    }
    if destination.tls {
        let mut tls = ClientTlsConfig::new().with_webpki_roots();
        if let Some(ca) = &config.tls_ca_pem {
            tls = tls.ca_certificate(Certificate::from_pem(ca));
        }
        if let (Some(certificate), Some(key)) = (
            &config.tls_client_certificate_pem,
            &config.tls_client_private_key_pem,
        ) {
            tls = tls.identity(Identity::from_pem(certificate, key.expose()));
        }
        if let Some(domain) = &config.tls_domain_name {
            tls = tls.domain_name(domain.clone());
        }
        endpoint = endpoint.tls_config(tls)?;
    }
    Ok(endpoint)
}

/// A client over `channel` with the configured message limits and compression.
pub(crate) fn client(channel: Channel, config: &GrpcSinkConfig) -> ChangeClient {
    let client = ChangeClient::new(channel)
        .max_encoding_message_size(config.max_request_bytes)
        .max_decoding_message_size(config.max_request_bytes);
    match config.compression {
        Compression::None => client,
        Compression::Gzip => client
            .send_compressed(CompressionEncoding::Gzip)
            .accept_compressed(CompressionEncoding::Gzip),
    }
}

/// How one failed call is handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// Not retried: the same request would fail the same way.
    Permanent,
    /// Retried within the retry policy.
    Retryable {
        /// Rebuild the channel before the next attempt.
        reconnect: bool,
        /// A delay the receiver requested.
        delay: Option<Duration>,
    },
}

/// Classifies a call failure by its gRPC status code, never by message text.
///
/// * Retried with a new channel: `UNAVAILABLE` (including connection
///   failures), `UNKNOWN` and `INTERNAL` (transport and HTTP/2 errors map to
///   these), `DEADLINE_EXCEEDED` and `CANCELLED` (tonic reports the expiry of
///   the call's own deadline as `CANCELLED`; the host never cancels a call
///   without dropping it). A fresh connection replaces one that may be
///   half-closed or draining after GOAWAY.
/// * Retried on the same channel: `RESOURCE_EXHAUSTED` and `ABORTED`, which
///   are receiver decisions on a healthy connection.
/// * Permanent: every other code, including `INVALID_ARGUMENT`,
///   `UNAUTHENTICATED`, `PERMISSION_DENIED`, `UNIMPLEMENTED`,
///   `FAILED_PRECONDITION`, `NOT_FOUND`, `ALREADY_EXISTS`, `OUT_OF_RANGE` and
///   `DATA_LOSS`.
///
/// `UNAVAILABLE` and `RESOURCE_EXHAUSTED` honour a requested delay from the
/// `grpc-retry-pushback-ms` (milliseconds) or `retry-after` (seconds or an
/// HTTP date) response metadata.
pub(crate) fn classify(status: &Status) -> Failure {
    let retry = |reconnect: bool| Failure::Retryable {
        reconnect,
        delay: matches!(status.code(), Code::Unavailable | Code::ResourceExhausted)
            .then(|| requested_delay(status.metadata()))
            .flatten(),
    };
    match status.code() {
        Code::Unavailable
        | Code::Unknown
        | Code::Internal
        | Code::DeadlineExceeded
        | Code::Cancelled => retry(true),
        Code::ResourceExhausted | Code::Aborted => retry(false),
        Code::Ok
        | Code::InvalidArgument
        | Code::NotFound
        | Code::AlreadyExists
        | Code::PermissionDenied
        | Code::FailedPrecondition
        | Code::OutOfRange
        | Code::Unimplemented
        | Code::DataLoss
        | Code::Unauthenticated => Failure::Permanent,
    }
}

/// A receiver-requested retry delay, if the response metadata carries one.
pub(crate) fn requested_delay(metadata: &tonic::metadata::MetadataMap) -> Option<Duration> {
    let text = |name: &str| metadata.get(name).and_then(|value| value.to_str().ok());
    if let Some(millis) = text("grpc-retry-pushback-ms").and_then(|v| v.trim().parse::<u64>().ok())
    {
        return Some(Duration::from_millis(millis));
    }
    text("retry-after").and_then(parse_retry_after)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_classified_without_reading_messages() {
        for code in [
            Code::Unavailable,
            Code::Unknown,
            Code::Internal,
            Code::DeadlineExceeded,
            Code::Cancelled,
        ] {
            assert_eq!(
                classify(&Status::new(code, "connection reset")),
                Failure::Retryable {
                    reconnect: true,
                    delay: None
                },
                "{code:?}"
            );
        }
        for code in [Code::ResourceExhausted, Code::Aborted] {
            assert!(matches!(
                classify(&Status::new(code, "")),
                Failure::Retryable {
                    reconnect: false,
                    ..
                }
            ));
        }
        for code in [
            Code::InvalidArgument,
            Code::Unauthenticated,
            Code::PermissionDenied,
            Code::Unimplemented,
            Code::FailedPrecondition,
            Code::NotFound,
            Code::AlreadyExists,
            Code::OutOfRange,
            Code::DataLoss,
        ] {
            assert_eq!(
                classify(&Status::new(code, "unavailable, retry")),
                Failure::Permanent,
                "{code:?}"
            );
        }
    }

    #[test]
    fn requested_delays_come_from_pushback_or_retry_after() {
        let mut metadata = tonic::metadata::MetadataMap::new();
        metadata.insert("grpc-retry-pushback-ms", "250".parse().expect("ascii"));
        let status = Status::with_metadata(Code::ResourceExhausted, "slow down", metadata);
        assert_eq!(
            classify(&status),
            Failure::Retryable {
                reconnect: false,
                delay: Some(Duration::from_millis(250))
            }
        );
        let mut metadata = tonic::metadata::MetadataMap::new();
        metadata.insert("retry-after", "2".parse().expect("ascii"));
        assert_eq!(requested_delay(&metadata), Some(Duration::from_secs(2)));
        let mut metadata = tonic::metadata::MetadataMap::new();
        metadata.insert("retry-after", "2".parse().expect("ascii"));
        let aborted = Status::with_metadata(Code::Aborted, "", metadata);
        assert_eq!(
            classify(&aborted),
            Failure::Retryable {
                reconnect: false,
                delay: None
            },
            "only UNAVAILABLE and RESOURCE_EXHAUSTED honour a requested delay"
        );
    }
}
