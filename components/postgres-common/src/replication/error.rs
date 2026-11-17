// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Typed failures for native PostgreSQL CDC. Each is permanent: retrying the
//! same activation cannot succeed, so reconnection never retries them. The
//! type is the shared [`drasi_computation_contracts::SourceFailure`].

/// The shared typed source failure, under its PostgreSQL name. Variants:
/// `SchemaChanged`, `PositionUnavailable`, `Unsupported` and `StateInUse`
/// (an owned replication slot is active).
pub use drasi_computation_contracts::SourceFailure as PostgresCdcError;

/// The server's host name did not resolve. Name service outages are
/// usually brief (a restarting pod behind a headless service returns
/// NXDOMAIN until it is back), so reconnection retries this within its
/// budget. The resolver's own error carries no portable kind.
#[derive(Debug)]
pub struct AddressUnresolved {
    pub host: String,
    pub source: std::io::Error,
}

impl std::fmt::Display for AddressUnresolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PostgreSQL host {} did not resolve", self.host)
    }
}

impl std::error::Error for AddressUnresolved {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

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
        }) || cause.is::<AddressUnresolved>()
            || cause.is::<tokio::time::error::Elapsed>()
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
        let unresolved = anyhow::Error::new(AddressUnresolved {
            host: "database.invalid".into(),
            source: std::io::Error::other("failed to lookup address information"),
        })
        .context("PostgreSQL reconnect failed");
        assert!(is_transient(&unresolved) && !is_permanent(&unresolved));
        assert!(!transient_sqlstate("42704") && !transient_sqlstate("28P01"));
    }
}

#[cfg(test)]
mod resolution_tests {
    use super::*;

    /// A name that cannot resolve fails as [`AddressUnresolved`], which
    /// reconnection retries, rather than as an unclassified permanent error.
    #[tokio::test]
    async fn an_unresolvable_host_is_a_transient_failure() {
        let config = crate::replication::PostgresTransactionConfig::new(
            "drasi-unresolvable.invalid",
            5432,
            "database",
            "user",
            "publication",
        );
        let error = match crate::replication::connect(&config).await {
            Ok(_) => panic!("an .invalid host must not resolve"),
            Err(error) => error,
        };
        assert!(
            error.chain().any(|cause| cause.is::<AddressUnresolved>()),
            "{error:#}"
        );
        assert!(is_transient(&error), "{error:#}");
    }
}
