// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Explicit batch-confirming HTTP delivery. Ordinary network factories do not
//! construct these services. The endpoint's handler still defines effect safety.

use anyhow::Result;
use async_trait::async_trait;
use axum::http::StatusCode;
use drasi_computation_contracts::{
    DeliveryBatchIdentity, DeliveryHandler, DeliveryItem, EnvelopeCodec, PortId,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::{num::NonZeroUsize, time::Duration};

pub const DELIVERY_PATH: &str = "/drasi/delivery/v1";
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 4096;

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
        message_limit(max_message_bytes)?;
        anyhow::ensure!(
            matches!(endpoint.scheme(), "http" | "https") && endpoint.fragment().is_none(),
            "invalid HTTP delivery URL"
        );
        anyhow::ensure!(
            !timeout.is_zero() && timeout <= Duration::from_secs(60),
            "HTTP delivery timeout must be positive and at most 60 seconds"
        );
        Ok(Self {
            client,
            endpoint,
            codec,
            max_message_bytes,
            timeout,
            confirmed: None,
        })
    }
}

#[async_trait]
impl DeliveryHandler for HttpDeliveryHandler {
    fn retryable(&self, error: &anyhow::Error) -> bool {
        matches!(error.downcast_ref::<HttpDeliveryError>(),
            Some(HttpDeliveryError::Rejected { status, .. })
                if *status == StatusCode::TOO_MANY_REQUESTS || *status == StatusCode::SERVICE_UNAVAILABLE)
            || error
                .downcast_ref::<reqwest::Error>()
                .is_some_and(|error| error.is_connect() || error.is_timeout())
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
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .timeout(self.timeout)
            .header("content-type", "application/json")
            .body(bytes)
            .send()
            .await?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(
                chunk.len() <= MAX_RESPONSE_BYTES - bytes.len(),
                "HTTP delivery response exceeds 4096 bytes"
            );
            bytes.extend_from_slice(&chunk);
        }
        if status != StatusCode::OK {
            return Err(HttpDeliveryError::Rejected {
                status,
                message: String::from_utf8_lossy(&bytes).into_owned(),
            }
            .into());
        }
        let confirmed: DeliveryBatchIdentity = serde_json::from_slice(&bytes)?;
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
