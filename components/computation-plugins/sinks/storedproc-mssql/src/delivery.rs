// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! SQL Server error classification for the shared stored-procedure delivery
//! protocol (`drasi_computation_storedproc_common::driver`). The protocol,
//! its cursor decisions, commit-outcome rules and the atomic skip live in the
//! shared crate; `session.rs` implements its `Session` over Tiberius.

use drasi_computation_storedproc_common::driver::ErrorClass;
pub use drasi_mssql_common::client::{ConnectTimeout, FailureClass};

/// Error number raised when first-stream admission or table creation cannot
/// take its application lock. Retryable on the same connection.
pub const ADMISSION_LOCK_ERROR: u32 = 59_101;
/// "Transaction count after EXECUTE indicates a mismatching number of BEGIN
/// and COMMIT statements": the procedure committed or rolled back.
pub const TRANSACTION_COUNT_MISMATCH: u32 = 266;

/// Classifies by type, SQL Server error number or I/O kind, never by text.
pub fn classify(error: &anyhow::Error) -> ErrorClass {
    for cause in error.chain() {
        if let Some(code) = cause
            .downcast_ref::<tiberius::error::Error>()
            .and_then(|error| match error {
                tiberius::error::Error::Server(token) => Some(token.code()),
                _ => None,
            })
        {
            return class(classify_server_code(code));
        }
        if let Some(class_) = drasi_mssql_common::client::classify_cause(cause) {
            return class(class_);
        }
    }
    ErrorClass::Permanent
}

fn class(class: FailureClass) -> ErrorClass {
    match class {
        FailureClass::Retryable { reconnect } => ErrorClass::Transient { reconnect },
        FailureClass::Permanent => ErrorClass::Permanent,
    }
}

/// Classifies a SQL Server error number: the shared rules, plus this sink's
/// admission-lock error, which is retryable on the same connection.
pub fn classify_server_code(code: u32) -> FailureClass {
    if code == ADMISSION_LOCK_ERROR {
        FailureClass::Retryable { reconnect: false }
    } else {
        drasi_mssql_common::client::classify_server_code(code)
    }
}

/// The SQL Server error number in `error`'s chain, if any.
pub fn server_code(error: &anyhow::Error) -> Option<u32> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<tiberius::error::Error>()
            .and_then(|error| match error {
                tiberius::error::Error::Server(token) => Some(token.code()),
                _ => None,
            })
    })
}
