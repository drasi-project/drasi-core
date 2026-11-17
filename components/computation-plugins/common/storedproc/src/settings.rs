// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Configuration rules shared by the stored-procedure sinks, so the same
//! setting means the same thing in each.
//!
//! | Setting | Meaning |
//! |---|---|
//! | `host`, `port`, `database`, `progressTable` | The destination cursor's identity: a change is unsupported in place (remove the sink with Erase and add a new one). |
//! | `progressTable` | `table` or `schema.table` ([`crate::ProgressTable`]); the dead-letter table is `<table>_dead_letter` beside it. |
//! | `maxStreams` | Cursor rows this sink may own, 1..=1,000,000 (default 4096). |
//! | `onPermanentFailure` | `stall` (default) or `skip`: a skipped operation is recorded in the dead-letter table atomically with the cursor advance. A change is a Replace. |
//! | `commandTimeoutMs` | Bound on each statement of an operation. |
//! | `connectTimeoutMs` | Bound on one connection attempt. |
//! | `shutdownTimeoutMs` | Bound on closing or retiring the session at Stop and Dispose when the host gives no deadline. |
//!
//! There is no in-sink retry budget: the graph retries transient failures.

use crate::cursor::{DEFAULT_MAX_STREAMS, MAX_STREAMS_LIMIT};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The default bound on closing or retiring the session.
pub const DEFAULT_SHUTDOWN_TIMEOUT_MS: u64 = 10_000;
/// The default bound on one statement.
pub const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 30_000;
/// The default bound on one connection attempt.
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 10_000;
/// Upper bound on every configured duration.
pub const MAX_DURATION_MS: u64 = 3_600_000;

pub fn default_max_streams() -> u32 {
    DEFAULT_MAX_STREAMS
}
pub fn default_shutdown_timeout_ms() -> u64 {
    DEFAULT_SHUTDOWN_TIMEOUT_MS
}
pub fn default_command_timeout_ms() -> u64 {
    DEFAULT_COMMAND_TIMEOUT_MS
}
pub fn default_connect_timeout_ms() -> u64 {
    DEFAULT_CONNECT_TIMEOUT_MS
}

/// Checks `maxStreams`.
pub fn validate_max_streams(max_streams: u32) -> Result<()> {
    ensure!(
        (1..=MAX_STREAMS_LIMIT).contains(&max_streams),
        "maxStreams must be in 1..={MAX_STREAMS_LIMIT}"
    );
    Ok(())
}

/// Checks a millisecond duration setting.
pub fn validate_duration(name: &str, value: u64) -> Result<Duration> {
    ensure!(
        (1..=MAX_DURATION_MS).contains(&value),
        "{name} must be in 1..={MAX_DURATION_MS}"
    );
    Ok(Duration::from_millis(value))
}

/// Transport security of a PostgreSQL or MySQL connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SslMode {
    /// Verified TLS: the server certificate chain must be trusted (system
    /// roots plus `tlsCaPem`) and must match `host` (verify-full).
    #[default]
    Require,
    /// Plaintext. Refused for a non-loopback host unless
    /// `allowRemotePlaintext` is set.
    Disable,
}

impl SslMode {
    /// The `sslMode` description shared by the sinks.
    pub const DESCRIPTION: &'static str = "require: verified TLS (trusted chain and matching host \
        name, verify-full). disable: plaintext, refused for a non-loopback host unless \
        allowRemotePlaintext is true.";
    /// The `allowRemotePlaintext` description shared by the sinks.
    pub const ALLOW_REMOTE_PLAINTEXT: &'static str = "Permit sslMode disable for a host that is \
        not localhost or a loopback address. Data and authentication then travel unencrypted.";
}

/// Secure by default: plaintext only to this machine unless explicitly
/// allowed.
pub fn validate_transport(
    ssl_mode: SslMode,
    allow_remote_plaintext: bool,
    host: &str,
) -> Result<()> {
    ensure!(
        ssl_mode == SslMode::Require || allow_remote_plaintext || is_loopback_host(host),
        "sslMode disable sends credentials and data unencrypted to the non-loopback host \
         '{host}'; use sslMode require or set allowRemotePlaintext"
    );
    Ok(())
}

/// Whether plaintext to `host` stays on this machine.
pub fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// The fields whose change moves the destination cursor. A sink's update
/// planning refuses to change them in place.
pub const CURSOR_IDENTITY: [&str; 4] = ["host", "port", "database", "progressTable"];

/// The unsupported-update reason for a changed cursor-identity field.
pub fn cursor_identity_changed(field: &str) -> String {
    format!(
        "{field} identifies where the destination cursor lives; changing it would move every \
         stream's cursor and lose its completed prefix (refused or repeated operations). Remove \
         the sink with Erase and add a new one instead"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_is_refused_for_remote_hosts_unless_allowed() {
        for host in ["localhost", "127.0.0.1", "::1", "[::1]"] {
            validate_transport(SslMode::Disable, false, host).unwrap();
        }
        assert!(validate_transport(SslMode::Disable, false, "db.example").is_err());
        validate_transport(SslMode::Disable, true, "db.example").unwrap();
        validate_transport(SslMode::Require, false, "db.example").unwrap();
    }
}
