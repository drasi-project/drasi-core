// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The activation-owned MySQL session of the stored-procedure sink.
//!
//! One persistent session carries every operation. A session whose command
//! did not finish (a timeout, a cancelled `handle`, a lost connection) is
//! retired with the session ownership proof: a separate control session
//! verifies the connection-attribute nonce and `KILL`s it, then waits until
//! the server no longer lists it. Only then can a replacement session run,
//! so an abandoned transaction can never commit or hold locks behind a retry.

use super::config::MySqlStoredProcConfig;
use crate::native::{
    connection::{Connection, MySqlServerError},
    session::{server_id, SessionCleanup},
    MySqlConnectionConfig, MySqlTlsMode,
};
use anyhow::{Context, Result};
use drasi_computation_contracts::lifecycle::CancellationToken;
use std::{
    collections::BTreeSet,
    num::{NonZeroU32, NonZeroU64},
    time::Duration,
};
use tokio::time::Instant;

/// The protocol-level I/O timeout of the session. Every command is bounded
/// by `commandTimeoutMs` (and every connect by `connectTimeoutMs`) around the
/// call instead, so those settings can change in place.
const SESSION_IO_TIMEOUT: Duration = Duration::from_secs(86_400);
const BACKOFF_BASE: Duration = Duration::from_millis(100);
const BACKOFF_CAP: Duration = Duration::from_secs(5);

/// The account and transport of one activation. Retained after Stop so
/// cleanup and `Erase` can reach the destination; never serialized.
#[derive(Clone)]
pub struct Endpoint {
    pub(crate) connection: MySqlConnectionConfig,
    pub(crate) max_bytes: usize,
}

impl Endpoint {
    pub fn new(config: &MySqlStoredProcConfig, user: String, password: String) -> Result<Self> {
        let connection = MySqlConnectionConfig {
            host: config.hostname.clone(),
            port: config.port,
            database: config.database.clone(),
            user,
            password,
            ssl_mode: if config.ssl {
                MySqlTlsMode::RequireVerifyFull
            } else {
                MySqlTlsMode::Disabled
            },
            tls_ca_pem: config.tls_ca_pem.clone(),
            tls_client_cert_pem: None,
            tls_client_key_pem: None,
            // A SQL session never registers as a replica; the shared
            // connection settings require these binlog-only values.
            server_id: NonZeroU32::MAX,
            heartbeat_interval_ms: NonZeroU64::MAX,
        };
        connection.validate(config.max_protocol_bytes)?;
        Ok(Self {
            connection,
            max_bytes: config.max_protocol_bytes,
        })
    }
}

/// Connect retry settings read per attempt.
#[derive(Debug, Clone, Copy)]
pub struct ConnectPolicy {
    pub connect_timeout: Duration,
    pub retries: u32,
    pub budget: Duration,
}

impl ConnectPolicy {
    pub fn from_config(config: &MySqlStoredProcConfig) -> Self {
        Self {
            connect_timeout: Duration::from_millis(config.connect_timeout_ms),
            retries: config.connect_retry_attempts,
            budget: Duration::from_millis(config.reconnect_budget_ms),
        }
    }
}

/// Connection failures worth another attempt: transport errors and timeouts,
/// server overload (1040 too many connections, 1203 user connection limit),
/// server shutdown (1053). Authentication and authorization fail at once.
/// Shared with the native source in [`crate::native::errors`].
pub use crate::native::errors::connect_retryable;

/// Full-jitter exponential backoff: uniform in `[0, min(cap, base * 2^n)]`.
pub fn backoff(attempt: u32) -> Duration {
    let ceiling = BACKOFF_BASE
        .saturating_mul(1u32 << attempt.min(16))
        .min(BACKOFF_CAP);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    let mixed = u64::from(nanos)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .rotate_left(17);
    ceiling.mul_f64((mixed % 10_000) as f64 / 10_000.0)
}

/// An established, ownership-verified session and its server identity.
pub struct Live {
    pub connection: Connection,
    pub server: String,
    /// Set before a command is sent and cleared once its reply was read. A
    /// dropped `handle` leaves it set, so the session is retired before reuse.
    pub busy: bool,
    /// Reused since the last completed operation (a dead idle session may be
    /// replaced once without retrying the operation through the host).
    pub idle: bool,
}

/// Opens one session with bounded, jittered retries within the policy
/// budget, the optional deadline and the optional cancellation.
pub async fn connect(
    endpoint: &Endpoint,
    policy: ConnectPolicy,
    cancellation: Option<&CancellationToken>,
    deadline: Option<Instant>,
) -> Result<Live> {
    let budget_end = Instant::now() + policy.budget;
    let end = deadline.map_or(budget_end, |deadline| deadline.min(budget_end));
    let mut attempt = 0u32;
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "MySQL connection budget (reconnectBudgetMs or the lifecycle deadline) expired"
        );
        let attempt_future = async {
            let mut connection = crate::native::session::connect(
                &endpoint.connection,
                endpoint.max_bytes,
                SESSION_IO_TIMEOUT,
            )
            .await?;
            match server_id(&mut connection).await {
                Ok(server) => Ok(Live {
                    connection,
                    server,
                    busy: false,
                    idle: false,
                }),
                Err(error) => {
                    let _ = tokio::time::timeout(Duration::from_secs(1), connection.close()).await;
                    Err(error)
                }
            }
        };
        let bounded = tokio::time::timeout(policy.connect_timeout.min(remaining), attempt_future);
        let result = match cancellation {
            Some(token) => tokio::select! {
                biased;
                _ = token.cancelled() => anyhow::bail!("MySQL connection cancelled"),
                result = bounded => result,
            },
            None => bounded.await,
        };
        let error = match result.context("MySQL connection attempt timed out") {
            Ok(Ok(live)) => return Ok(live),
            Ok(Err(error)) | Err(error) => error,
        };
        if !connect_retryable(&error) || attempt >= policy.retries {
            return Err(error.context(format!(
                "MySQL connection to {}:{} failed after {} attempt(s)",
                endpoint.connection.host,
                endpoint.connection.port,
                attempt + 1
            )));
        }
        let delay = backoff(attempt).min(end.saturating_duration_since(Instant::now()));
        log::warn!(
            "MySQL stored-procedure sink connection attempt {} failed; retrying in {:?}: {error:#}",
            attempt + 1,
            delay
        );
        match cancellation {
            Some(token) => tokio::select! {
                biased;
                _ = token.cancelled() => anyhow::bail!("MySQL connection cancelled"),
                _ = tokio::time::sleep(delay) => {}
            },
            None => tokio::time::sleep(delay).await,
        }
        attempt += 1;
    }
}

/// The activation's session plus any retired sessions not yet proven gone.
#[derive(Default)]
pub struct Sessions {
    pub live: Option<Live>,
    retiring: Vec<Connection>,
    server: Option<String>,
    killed: BTreeSet<u32>,
}

impl Sessions {
    /// Whether retired sessions still await proof that they are gone.
    pub fn cleanup_pending(&self) -> bool {
        !self.retiring.is_empty()
    }

    /// Whether anything is open or pending.
    pub fn is_empty(&self) -> bool {
        self.live.is_none() && self.retiring.is_empty()
    }

    /// Moves the live session to the retiring set.
    pub fn retire_live(&mut self) {
        if let Some(live) = self.live.take() {
            self.server = Some(live.server);
            self.retiring.push(live.connection);
        }
    }

    /// Proves every retiring session gone (KILL through a control session
    /// after verifying its ownership nonce) within `timeout`. On timeout or
    /// failure the sessions stay owned for a later attempt.
    pub async fn retire(&mut self, endpoint: &Endpoint, timeout: Duration) -> Result<()> {
        if self.retiring.is_empty() {
            return Ok(());
        }
        let mut control = None;
        let outcome = tokio::time::timeout(
            timeout,
            SessionCleanup {
                config: &endpoint.connection,
                max_bytes: endpoint.max_bytes,
                timeout: timeout.min(Duration::from_secs(30)),
                control: &mut control,
                sessions: &mut self.retiring,
                server: &self.server,
                killed: &mut self.killed,
            }
            .cleanup(),
        )
        .await;
        if let Some(control) = control {
            let _ = tokio::time::timeout(Duration::from_secs(1), control.close()).await;
        }
        if self.retiring.is_empty() {
            // Every session was killed or is no longer listed; a failure to
            // shut down an already dead socket does not leave it owned.
            if let Ok(Err(error)) = outcome {
                log::debug!("MySQL retired session close reported: {error:#}");
            }
            self.killed.clear();
            return Ok(());
        }
        match outcome {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.context("MySQL session cleanup is pending")),
            Err(_) => anyhow::bail!("MySQL session cleanup did not finish in time; retry"),
        }
    }

    /// Closes an idle live session (no transaction is open) without KILL.
    pub async fn close_idle(&mut self, timeout: Duration) {
        if let Some(live) = self.live.take() {
            if let Ok(Err(error)) | Err(error) =
                tokio::time::timeout(timeout, live.connection.close())
                    .await
                    .map_err(anyhow::Error::from)
            {
                log::debug!("MySQL session close reported: {error:#}");
            }
        }
    }

    /// Releases ownership without cleanup (object dropped).
    pub fn abandoned(&self) -> bool {
        self.live.as_ref().is_some_and(|live| live.busy) || !self.retiring.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_bounded_full_jitter() {
        for attempt in 0..40 {
            let delay = backoff(attempt);
            assert!(delay <= BACKOFF_CAP, "{attempt}: {delay:?}");
            assert!(delay <= BACKOFF_BASE.saturating_mul(1 << attempt.min(16)));
        }
    }

    #[test]
    fn connect_classification_is_by_kind_and_code() {
        let server = |code| -> anyhow::Error {
            MySqlServerError {
                code,
                sql_state: None,
                message: "m".into(),
            }
            .into()
        };
        assert!(connect_retryable(&server(1040)));
        assert!(connect_retryable(&server(1203)));
        assert!(!connect_retryable(&server(1045)));
        assert!(!connect_retryable(&server(1049)));
        assert!(connect_retryable(&std::io::Error::other("refused").into()));
        assert!(!connect_retryable(&anyhow::anyhow!("protocol violation")));
    }
}
