// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Typed failures for native PostgreSQL CDC. Each is permanent: retrying the
//! same activation cannot succeed, so reconnection never retries them.

use std::fmt;

/// A permanent native PostgreSQL CDC failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostgresCdcError {
    /// A selected table, its key, its replica identity or the publication
    /// changed incompatibly with the committed cursor's binding. Remove the
    /// source (with erase) and add it again to rebuild from a new snapshot.
    SchemaChanged(String),
    /// The committed cursor is no longer retained by the server: the slot is
    /// missing or invalidated, or its WAL was removed.
    PositionUnavailable(String),
    /// The server or publication cannot be used by this source.
    Unsupported(String),
    /// A slot that must be idle is in use by another connection.
    SlotActive(String),
}

impl fmt::Display for PostgresCdcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SchemaChanged(detail) => write!(
                f,
                "PostgreSQL schema changed incompatibly ({detail}); remove the source with erase and add it again to rebuild"
            ),
            Self::PositionUnavailable(detail) => write!(
                f,
                "PostgreSQL position is no longer available ({detail}); explicit recovery is required"
            ),
            Self::Unsupported(detail) => write!(f, "unsupported PostgreSQL configuration: {detail}"),
            Self::SlotActive(slot) => write!(
                f,
                "PostgreSQL replication slot {slot} is in use by another connection"
            ),
        }
    }
}

impl std::error::Error for PostgresCdcError {}

/// Whether `error` is, or wraps, a permanent [`PostgresCdcError`].
pub fn is_permanent(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<PostgresCdcError>())
}

/// Whether `error` is a transport failure that a new connection may cure:
/// an I/O failure, a timeout, or a server shutdown/connection SQLSTATE.
pub fn is_transient(error: &anyhow::Error) -> bool {
    use std::io::ErrorKind;
    if is_permanent(error) {
        return false;
    }
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                ErrorKind::ConnectionRefused
                    | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::NotConnected
                    | ErrorKind::BrokenPipe
                    | ErrorKind::TimedOut
                    | ErrorKind::UnexpectedEof
                    | ErrorKind::HostUnreachable
                    | ErrorKind::NetworkUnreachable
            )
        }) || cause.is::<tokio::time::error::Elapsed>()
            || cause
                .downcast_ref::<crate::protocol::ErrorResponse>()
                .is_some_and(|error| transient_sqlstate(&error.code))
    })
}

/// Server shutdown, crash recovery, connection failures and a slot briefly
/// held by a terminating walsender (55006).
pub fn transient_sqlstate(code: &str) -> bool {
    matches!(
        code,
        "57P01" | "57P02" | "57P03" | "08000" | "08001" | "08003" | "08006" | "55006"
    ) || code.starts_with("53")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_separates_transport_failures_from_permanent_ones() {
        assert!(is_transient(
            &std::io::Error::new(std::io::ErrorKind::ConnectionReset, "closed").into()
        ));
        assert!(is_transient(
            &anyhow::Error::from(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "eof"
            ))
            .context("PostgreSQL replication failed")
        ));
        assert!(!is_transient(
            &std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied").into()
        ));
        assert!(!is_transient(&anyhow::anyhow!("changed source binding")));
        let permanent = anyhow::Error::new(PostgresCdcError::SchemaChanged("key".into())).context(
            std::io::Error::new(std::io::ErrorKind::ConnectionReset, "x"),
        );
        assert!(is_permanent(&permanent));
        assert!(!is_transient(&permanent));
        assert!(transient_sqlstate("57P01") && transient_sqlstate("53300"));
        assert!(!transient_sqlstate("42704") && !transient_sqlstate("28P01"));
    }
}
