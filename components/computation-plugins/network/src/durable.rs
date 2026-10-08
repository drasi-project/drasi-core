// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::ingress::Ingress;
use drasi_computation_plugin_sdk::{AdmissionError, AdmissionErrorKind, NativeAdmission};
use drasi_core::models::SourceChange;
use drasi_lib::computation::v1::{
    AdmissionReceipt, ChangeEnvelope, GraphChangeCodec, ProducerSession,
};
use serde::{Deserialize, Serialize};
use std::{future::Future, time::Duration};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchOutcome {
    pub receipts: Vec<AdmissionReceipt>,
    pub error: Option<AdmissionError>,
}

pub(crate) fn invalid(error: impl std::fmt::Display) -> AdmissionError {
    AdmissionError {
        kind: AdmissionErrorKind::Invalid,
        message: error.to_string(),
    }
}

impl Ingress {
    pub fn admission_service(&self) -> Result<&NativeAdmission, AdmissionError> {
        self.durable().ok_or_else(|| AdmissionError {
            kind: AdmissionErrorKind::NotEnabled,
            message: "source has no durable admission binding".into(),
        })
    }

    pub async fn admission_call<T>(
        &self,
        uncertainty: AdmissionErrorKind,
        operation: impl Future<Output = Result<T, AdmissionError>>,
    ) -> Result<T, AdmissionError> {
        if self.cancel.is_cancelled() {
            return Err(AdmissionError {
                kind: AdmissionErrorKind::Closed,
                message: "source stopped".into(),
            });
        }
        let resolution = match uncertainty {
            AdmissionErrorKind::MetadataUnknown => {
                "retry registration with the same producer name or retirement with the same session"
            }
            AdmissionErrorKind::AcceptanceUnknown => {
                "resolve the same session and sequence before advancing"
            }
            _ => "retry the read after the source is available",
        };
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(AdmissionError { kind: uncertainty,
                message: format!("source stopped during request; {resolution}") }),
            result = tokio::time::timeout(Duration::from_millis(self.config.timeout_ms), operation) =>
                result.unwrap_or_else(|_| Err(AdmissionError { kind: uncertainty,
                    message: format!("admission deadline elapsed; {resolution}") })),
        }
    }

    pub fn durable_envelope(
        &self,
        change: SourceChange,
        sequence: u64,
    ) -> Result<ChangeEnvelope, AdmissionError> {
        // No receiving-side clock or volatile producer identity participates in
        // the retry digest. The client event's effective timestamp is preserved.
        let timestamp = chrono::DateTime::from_timestamp_millis(
            i64::try_from(change.get_transaction_time()).map_err(invalid)?,
        )
        .ok_or_else(|| invalid("event timestamp is out of range"))?;
        GraphChangeCodec::encode_change(
            change,
            self.config.stream.clone(),
            sequence,
            Some(timestamp),
        )
        .map_err(invalid)
    }

    pub async fn admit_durable(
        &self,
        session: &ProducerSession,
        first_sequence: u64,
        changes: Vec<SourceChange>,
    ) -> Result<BatchOutcome, AdmissionError> {
        let service = self.admission_service()?;
        if changes.is_empty()
            || first_sequence == 0
            || first_sequence
                .checked_add(u64::try_from(changes.len() - 1).map_err(invalid)?)
                .is_none()
        {
            return Err(invalid(
                "nonempty batch and nonzero, non-overflowing sequence range required",
            ));
        }
        let envelopes = changes
            .into_iter()
            .enumerate()
            .map(|(offset, change)| self.durable_envelope(change, first_sequence + offset as u64))
            .collect::<Result<Vec<_>, _>>()?;
        let mut receipts = Vec::with_capacity(envelopes.len());
        let result = self
            .admission_call(AdmissionErrorKind::AcceptanceUnknown, async {
                for (offset, envelope) in envelopes.iter().enumerate() {
                    receipts.push(
                        service
                            .admit(session, first_sequence + offset as u64, envelope)
                            .await?,
                    );
                }
                Ok(())
            })
            .await;
        Ok(BatchOutcome {
            receipts,
            error: result.err(),
        })
    }
}
