// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Failure classification shared by the native MySQL source, its bootstrap
//! providers and the stored-procedure sink.
//!
//! Classification is by type and MySQL error number, never by message text.
//! Permanent source failures are the shared typed
//! [`drasi_computation_contracts::SourceFailure`]; they are never retried.

use super::connection::MySqlServerError;
pub use drasi_computation_contracts::{is_permanent_source_failure, SourceFailure};

/// Bounds chain walks so a pathological error cannot loop forever.
const CHAIN_LIMIT: usize = 64;

/// The first MySQL server error in `error`'s chain.
pub fn server_error(error: &anyhow::Error) -> Option<&MySqlServerError> {
    error
        .chain()
        .take(CHAIN_LIMIT)
        .find_map(|cause| cause.downcast_ref::<MySqlServerError>())
}

/// A transport-level failure: an I/O error or an elapsed timeout.
fn transport(error: &anyhow::Error) -> bool {
    error.chain().take(CHAIN_LIMIT).any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some()
            || cause.is::<tokio::time::error::Elapsed>()
            || cause.is::<DeadPeer>()
    })
}

/// Server errors a new connection may cure:
///
/// - 1040 too many connections, 1203 per-user connection limit;
/// - 1053 server shutdown, 1077 normal shutdown, 1078/1079 shutdown complete;
/// - 1152/1153/1154/1155/1156/1157/1158/1159/1160/1161 network read/write
///   errors and timeouts reported by the server;
/// - 1927 connection killed (`KILL CONNECTION`), 3169 session killed;
/// - 4031 disconnected by the server because of inactivity.
pub fn transient_server_code(code: u16) -> bool {
    matches!(
        code,
        1040 | 1053 | 1077 | 1078 | 1079 | 1152..=1161 | 1203 | 1927 | 3169 | 4031
    )
}

/// Connection failures worth another attempt: transport errors and timeouts,
/// server overload and server shutdown. Authentication and authorization
/// fail at once. Used by the stored-procedure sink's connect loop.
pub fn connect_retryable(error: &anyhow::Error) -> bool {
    match server_error(error) {
        Some(error) => matches!(error.code, 1040 | 1053 | 1203),
        None => transport(error),
    }
}

/// Whether a replication failure may be cured by reconnecting at the last
/// emitted boundary. Permanent [`SourceFailure`]s are never transient.
pub fn is_transient(error: &anyhow::Error) -> bool {
    if is_permanent_source_failure(error) {
        return false;
    }
    match server_error(error) {
        Some(server) => transient_server_code(server.code),
        None => transport(error),
    }
}

/// A permanent schema-change failure.
pub fn schema_changed(detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(SourceFailure::SchemaChanged(detail.into()))
}

/// A permanent "server cannot be used" failure.
pub fn unsupported(detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(SourceFailure::Unsupported(detail.into()))
}

/// No binlog event or heartbeat arrived within the dead-peer window.
#[derive(Debug)]
pub struct DeadPeer(pub std::time::Duration);

impl std::fmt::Display for DeadPeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no MySQL binlog event or heartbeat within {:?}; the connection is presumed dead",
            self.0
        )
    }
}

impl std::error::Error for DeadPeer {}

/// Maps a binlog-dump refusal (error 1236, `ER_MASTER_FATAL_ERROR_READING_BINLOG`)
/// to a permanent failure. MySQL uses it for purged or missing history and for
/// a replica that collides with another replica's server ID.
pub fn classify_dump_error(error: anyhow::Error) -> anyhow::Error {
    let Some(server) = server_error(&error) else {
        return error;
    };
    if server.code != 1236 {
        return error;
    }
    let detail = format!(
        "MySQL refused binlog streaming (error 1236: {}); the history is purged or another \
         replica uses the same serverId",
        server.message
    );
    anyhow::Error::new(SourceFailure::PositionUnavailable(detail)).context(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(code: u16) -> anyhow::Error {
        MySqlServerError {
            code,
            sql_state: None,
            message: "m".into(),
        }
        .into()
    }

    #[test]
    fn transport_and_server_codes_are_classified_by_type() {
        assert!(is_transient(
            &std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "closed").into()
        ));
        assert!(is_transient(
            &anyhow::Error::new(DeadPeer(std::time::Duration::from_secs(1))).context("x")
        ));
        assert!(is_transient(&server(1053)));
        assert!(is_transient(&server(1927).context("replication")));
        assert!(!is_transient(&server(1045)));
        assert!(!is_transient(&server(1236)));
        assert!(!is_transient(&anyhow::anyhow!("protocol violation")));
        let permanent = schema_changed("key").context(std::io::Error::other("x"));
        assert!(is_permanent_source_failure(&permanent));
        assert!(!is_transient(&permanent));
    }

    #[test]
    fn connect_retry_keeps_the_sink_contract() {
        assert!(connect_retryable(&server(1040)));
        assert!(connect_retryable(&server(1203)));
        assert!(!connect_retryable(&server(1045)));
        assert!(!connect_retryable(&server(1049)));
        assert!(connect_retryable(&std::io::Error::other("refused").into()));
        assert!(!connect_retryable(&anyhow::anyhow!("protocol violation")));
    }

    #[test]
    fn dump_refusals_become_permanent() {
        let classified = classify_dump_error(server(1236));
        assert!(is_permanent_source_failure(&classified));
        assert!(matches!(
            drasi_computation_contracts::source_failure(&classified),
            Some(SourceFailure::PositionUnavailable(_))
        ));
        assert!(!is_permanent_source_failure(&classify_dump_error(server(
            1053
        ))));
    }
}
