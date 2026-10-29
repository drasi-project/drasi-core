// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_lib::computation::v1::{AdmissionRejection, PipeError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Invalid,
    Incompatible,
    Busy,
    Conflict,
    Expired,
    Gap,
    StateLost,
    StaleGeneration,
    Closed,
    Sealed,
    AcceptanceUnknown,
    Storage,
}

/// A wire-safe error, never a successful EOF or an implied rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(deny_unknown_fields)]
#[error("{code:?}: {message}")]
pub struct BridgeError {
    pub code: ErrorCode,
    pub message: String,
}

impl BridgeError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > 512 {
            let mut end = 512;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
        }
        Self { code, message }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Invalid, message)
    }

    pub(crate) fn unknown(error: impl std::fmt::Display) -> Self {
        Self::new(ErrorCode::AcceptanceUnknown, error.to_string())
    }

    pub fn retryable(&self) -> bool {
        matches!(
            self.code,
            ErrorCode::Busy
                | ErrorCode::Closed
                | ErrorCode::AcceptanceUnknown
                | ErrorCode::StaleGeneration
        )
    }
}

impl From<PipeError> for BridgeError {
    fn from(error: PipeError) -> Self {
        let code = match &error {
            PipeError::CapacityExhausted => ErrorCode::Busy,
            PipeError::Closed => ErrorCode::Closed,
            PipeError::AcceptanceUnknown { .. } | PipeError::AcknowledgementUnknown { .. } => {
                ErrorCode::AcceptanceUnknown
            }
            PipeError::Backend(error) => match error.downcast_ref::<AdmissionRejection>() {
                Some(AdmissionRejection::Busy) => ErrorCode::Busy,
                Some(AdmissionRejection::SessionExpired) => ErrorCode::StateLost,
                Some(AdmissionRejection::ReceiptExpired(_)) => ErrorCode::Expired,
                Some(AdmissionRejection::Sequence { .. }) => ErrorCode::Gap,
                Some(AdmissionRejection::PayloadConflict(_)) => ErrorCode::Conflict,
                _ => ErrorCode::Storage,
            },
            _ => ErrorCode::Invalid,
        };
        Self::new(code, error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, BridgeError>;
