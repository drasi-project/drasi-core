// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::IndexError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum MiddlewareError {
    #[error("Error processing source change: {0}")]
    SourceChangeError(String),

    #[error("Unknown middleware {0}")]
    UnknownKind(String),

    #[error("Error processing source change: {0}")]
    IndexError(IndexError),
}

impl From<IndexError> for MiddlewareError {
    fn from(val: IndexError) -> Self {
        MiddlewareError::IndexError(val)
    }
}

#[derive(Error, Debug)]
pub enum MiddlewareSetupError {
    #[error("Invalid configuration: {0}")]
    InvalidConfiguration(String),
    #[error("No registry found for middleware")]
    NoRegistry,
}
