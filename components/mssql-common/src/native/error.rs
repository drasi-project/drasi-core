// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Typed permanent failures of the native SQL Server CDC engine. Each wraps
//! a [`SourceFailure`], so retry loops stop on it and hosts classify it by
//! variant; transport failures are classified separately by
//! [`crate::client::classify`].

use drasi_computation_contracts::SourceFailure;

/// A selected table, capture instance or bound key fact changed.
pub fn schema_changed(detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(SourceFailure::SchemaChanged(format!(
        "SQL Server: {}",
        detail.into()
    )))
}

/// The committed cursor is no longer retained by CDC.
pub fn position_unavailable(detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(SourceFailure::PositionUnavailable(format!(
        "SQL Server: {}",
        detail.into()
    )))
}

/// The server, its CDC configuration or the login cannot be used.
pub fn unsupported(detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(SourceFailure::Unsupported(format!(
        "SQL Server: {}",
        detail.into()
    )))
}

/// Whether a reconnect may cure `error`: a transport failure by type or
/// SQL Server error number, and never a typed [`SourceFailure`].
pub fn is_transient(error: &anyhow::Error) -> bool {
    !drasi_computation_contracts::is_permanent_source_failure(error)
        && crate::client::classify(error).is_retryable()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_failures_never_reconnect_and_transport_failures_do() {
        let io = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        assert!(is_transient(&io));
        for error in [
            schema_changed("x"),
            position_unavailable("x"),
            unsupported("x"),
            anyhow::anyhow!("plain"),
        ] {
            assert!(!is_transient(&error));
        }
    }
}
