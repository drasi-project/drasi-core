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
    DeliveryBatchIdentity, DeliveryHandler, DeliveryItem, EnvelopeCodec, PortId,
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
        };
        handler.set_options(max_message_bytes, timeout, None)?;
        Ok(handler)
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

/// Whether a delivery failure may succeed when the runner retries it:
/// transport failures (connect, timeout, in flight) and 408, 425, 429 and
/// 5xx rejections. A confirmation mismatch or another rejection is permanent.
pub fn retryable(error: &anyhow::Error) -> bool {
    match error.downcast_ref::<HttpDeliveryError>() {
        Some(HttpDeliveryError::Rejected { status, .. }) => {
            status.is_server_error() || matches!(status.as_u16(), 408 | 425 | 429)
        }
        Some(HttpDeliveryError::ConfirmationMismatch) => false,
        None => error
            .downcast_ref::<reqwest::Error>()
            .is_some_and(crate::transport::transport_retryable),
    }
}

#[async_trait]
impl DeliveryHandler for HttpDeliveryHandler {
    fn retryable(&self, error: &anyhow::Error) -> bool {
        retryable(error)
    }

    async fn handle(&mut self, item: DeliveryItem<'_>) -> Result<()> {
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
            .map_err(reqwest::Error::without_url)?;
        let status = response.status();
        if status != StatusCode::OK {
            let body = crate::transport::read_capped(response, MAX_RESPONSE_BYTES).await;
            return Err(HttpDeliveryError::Rejected {
                status,
                message: sanitize(&String::from_utf8_lossy(&body)),
            }
            .into());
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
        let confirmed: DeliveryBatchIdentity =
            serde_json::from_slice(&bytes).map_err(|_| HttpDeliveryError::ConfirmationMismatch)?;
        if confirmed != identity {
            return Err(HttpDeliveryError::ConfirmationMismatch.into());
        }
        self.confirmed = Some(confirmed);
        Ok(())
    }
}

#[cfg(feature = "delivery-receiver")]
mod receiver;
#[cfg(feature = "delivery-receiver")]
pub use receiver::HttpDeliveryEndpoint;
