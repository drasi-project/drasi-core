// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Batch-confirming HTTP delivery for the host `DeliveryRunner`
//! (`ConsumerMode::External`). The `drasi.network/http-delivery` factory
//! constructs [`HttpDeliveryHandler`]; the receiving endpoint's handler still
//! defines effect safety.

use anyhow::Result;
use async_trait::async_trait;
use axum::http::StatusCode;
use drasi_computation_contracts::{
    DeliveryBatchIdentity, DeliveryFailure, DeliveryHandler, DeliveryItem, EnvelopeCodec,
    PermanentFailurePolicy, PortId,
};
use drasi_computation_network_common::config::{sanitize, SecretString, DELIVERY_ID_HEADER};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::{num::NonZeroUsize, time::Duration};

/// Path served by the reference receiver ([`HttpDeliveryEndpoint`] with the
/// `delivery-receiver` feature).
pub const DELIVERY_PATH: &str = "/drasi/delivery/v1";
/// Upper bound on one delivery request.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// Upper bound on one delivery attempt's timeout.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60);
/// Largest confirmation or rejection body read from the endpoint.
pub const MAX_RESPONSE_BYTES: usize = 4096;

fn message_limit(value: NonZeroUsize) -> Result<()> {
    anyhow::ensure!(
        value.get() <= MAX_MESSAGE_BYTES,
        "HTTP delivery messages must not exceed 16 MiB"
    );
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    version: u32,
    port: PortId,
    identity: DeliveryBatchIdentity,
    envelope: Box<RawValue>,
}

#[derive(Debug, thiserror::Error)]
pub enum HttpDeliveryError {
    /// `message` is the endpoint's (sanitized, truncated) response body.
    #[error("delivery endpoint rejected the batch ({status}): {message}")]
    Rejected { status: StatusCode, message: String },
    #[error("delivery endpoint did not confirm the exact completed batch")]
    ConfirmationMismatch,
}

/// Sends the full batch once, not once per operation. Only an exact whole-batch
/// confirmation lets subsequent local operations complete without another request.
pub struct HttpDeliveryHandler {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    codec: EnvelopeCodec,
    max_message_bytes: NonZeroUsize,
    timeout: Duration,
    authorization: Option<reqwest::header::HeaderValue>,
    confirmed: Option<DeliveryBatchIdentity>,
    policy: PermanentFailurePolicy,
    /// The last batch the endpoint rejected permanently under `skip`, with
    /// the rejection: its remaining operations are skipped without resending.
    rejected: Option<(DeliveryBatchIdentity, String)>,
}

impl HttpDeliveryHandler {
    pub fn new(
        client: reqwest::Client,
        endpoint: reqwest::Url,
        codec: EnvelopeCodec,
        max_message_bytes: NonZeroUsize,
        timeout: Duration,
    ) -> Result<Self> {
        anyhow::ensure!(
            matches!(endpoint.scheme(), "http" | "https") && endpoint.fragment().is_none(),
            "invalid HTTP delivery URL"
        );
        let mut handler = Self {
            client,
            endpoint,
            codec,
            max_message_bytes,
            timeout,
            authorization: None,
            confirmed: None,
            policy: PermanentFailurePolicy::Stall,
            rejected: None,
        };
        handler.set_options(max_message_bytes, timeout, None)?;
        Ok(handler)
    }

    /// The declared permanent-failure policy. Under `skip`, a batch the
    /// endpoint rejects permanently is settled as skipped
    /// ([`DeliveryFailure::skipped`]): the host `DeliveryRunner`'s ledger,
    /// this consumer's only progress, records each of its operations as
    /// completed, atomically, so the skip is never repeated on replay.
    pub fn with_permanent_failure(mut self, policy: PermanentFailurePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Replaces the per-request settings, keeping the client, endpoint and
    /// the last confirmed batch.
    pub fn set_options(
        &mut self,
        max_message_bytes: NonZeroUsize,
        timeout: Duration,
        token: Option<&SecretString>,
    ) -> Result<()> {
        message_limit(max_message_bytes)?;
        anyhow::ensure!(
            !timeout.is_zero() && timeout <= MAX_TIMEOUT,
            "HTTP delivery timeout must be positive and at most 60 seconds"
        );
        let authorization = token
            .map(|token| {
                let mut value =
                    reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.expose()))?;
                value.set_sensitive(true);
                anyhow::Ok(value)
            })
            .transpose()?;
        self.max_message_bytes = max_message_bytes;
        self.timeout = timeout;
        self.authorization = authorization;
        Ok(())
    }
}

/// Classifies a failed batch delivery with the same rules as the HTTP sink
/// ([`crate::transport::transient_status`]): transport failures and 5xx, 408,
/// 409, 425 and 429 rejections are transient, honouring `Retry-After`;
/// another rejection or a confirmation mismatch is permanent.
fn classify(error: anyhow::Error, requested: Option<Duration>) -> anyhow::Error {
    let transient = match error.downcast_ref::<HttpDeliveryError>() {
        Some(HttpDeliveryError::Rejected { status, .. }) => {
            crate::transport::transient_status(*status)
        }
        Some(HttpDeliveryError::ConfirmationMismatch) => false,
        None => error
            .downcast_ref::<reqwest::Error>()
            .is_none_or(crate::transport::transport_retryable),
    };
    let failure = match (transient, requested) {
        (true, Some(delay)) => DeliveryFailure::transient_after(error, delay),
        (true, None) => DeliveryFailure::transient(error),
        (false, _) => DeliveryFailure::permanent(error),
    };
    failure.into()
}

#[async_trait]
impl DeliveryHandler for HttpDeliveryHandler {
    async fn handle(&mut self, item: DeliveryItem<'_>) -> Result<()> {
        let identity = item.batch_identity()?;
        if let Some((rejected, reason)) = &self.rejected {
            if *rejected == identity {
                return Err(DeliveryFailure::skipped(anyhow::anyhow!(
                    "the delivery endpoint rejected this batch: {reason}"
                ))
                .into());
            }
        }
        let result = self.send(item).await;
        match result {
            Err(error)
                if self.policy == PermanentFailurePolicy::Skip
                    && matches!(
                        error
                            .downcast_ref::<DeliveryFailure>()
                            .map(DeliveryFailure::class),
                        Some(drasi_computation_contracts::DeliveryFailureClass::Permanent)
                    )
                    && error.chain().any(|cause| {
                        matches!(
                            cause.downcast_ref(),
                            Some(HttpDeliveryError::Rejected { .. })
                        )
                    }) =>
            {
                self.rejected = Some((identity, format!("{error:#}")));
                Err(DeliveryFailure::skipped(error).into())
            }
            result => result,
        }
    }
}

impl HttpDeliveryHandler {
    async fn send(&mut self, item: DeliveryItem<'_>) -> Result<()> {
        let identity = item.batch_identity()?;
        if self.confirmed.as_ref() == Some(&identity) {
            return Ok(());
        }
        let batch = Batch {
            version: 1,
            port: item.port.clone(),
            identity: identity.clone(),
            envelope: RawValue::from_string(String::from_utf8(
                self.codec.encode(item.envelope)?.to_vec(),
            )?)?,
        };
        let bytes = serde_json::to_vec(&batch)?;
        anyhow::ensure!(
            bytes.len() <= self.max_message_bytes.get(),
            "HTTP delivery request exceeds its configured limit"
        );
        // The batch identity is stable across retries and replays, so the
        // endpoint can deduplicate on it.
        let delivery_id = format!("{}/{}", identity.stream(), identity.sequence());
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .timeout(self.timeout)
            .header("content-type", "application/json")
            .header(DELIVERY_ID_HEADER, delivery_id);
        if let Some(authorization) = &self.authorization {
            request = request.header(reqwest::header::AUTHORIZATION, authorization.clone());
        }
        let mut response = request
            .body(bytes)
            .send()
            .await
            .map_err(|error| classify(error.without_url().into(), None))?;
        let status = response.status();
        if status != StatusCode::OK {
            let requested = crate::transport::requested_delay(&response);
            let body = crate::transport::read_capped(response, MAX_RESPONSE_BYTES).await;
            return Err(classify(
                HttpDeliveryError::Rejected {
                    status,
                    message: sanitize(&String::from_utf8_lossy(&body)),
                }
                .into(),
                requested,
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(reqwest::Error::without_url)?
        {
            anyhow::ensure!(
                chunk.len() <= MAX_RESPONSE_BYTES - bytes.len(),
                "HTTP delivery confirmation exceeds {MAX_RESPONSE_BYTES} bytes"
            );
            bytes.extend_from_slice(&chunk);
        }
        let confirmed: DeliveryBatchIdentity = serde_json::from_slice(&bytes)
            .map_err(|_| classify(HttpDeliveryError::ConfirmationMismatch.into(), None))?;
        if confirmed != identity {
            return Err(classify(
                HttpDeliveryError::ConfirmationMismatch.into(),
                None,
            ));
        }
        self.confirmed = Some(confirmed);
        Ok(())
    }
}

#[cfg(feature = "delivery-receiver")]
mod receiver;
#[cfg(feature = "delivery-receiver")]
pub use receiver::HttpDeliveryEndpoint;
