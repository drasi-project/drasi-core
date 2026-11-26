// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_plugin_sdk::{AdmissionError, AdmissionErrorKind};
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

impl From<AdmissionError> for BridgeError {
    fn from(error: AdmissionError) -> Self {
        use AdmissionErrorKind as Kind;
        let code = match error.kind {
            Kind::Busy | Kind::CapacityExhausted => ErrorCode::Busy,
            Kind::Closed => ErrorCode::Closed,
            Kind::AcceptanceUnknown | Kind::MetadataUnknown => ErrorCode::AcceptanceUnknown,
            Kind::SessionExpired => ErrorCode::StateLost,
            Kind::ReceiptExpired => ErrorCode::Expired,
            Kind::Sequence => ErrorCode::Gap,
            Kind::PayloadConflict => ErrorCode::Conflict,
            Kind::NotEnabled | Kind::PendingObligations | Kind::IdentifierTooLong => {
                ErrorCode::Storage
            }
            Kind::Invalid => ErrorCode::Invalid,
        };
        Self::new(code, error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, BridgeError>;
