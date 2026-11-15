// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! One activation-owned PostgreSQL connection and its driver task.

use crate::config::{PostgresStoredProcConfig, SslMode};
use anyhow::Context;
use drasi_postgres_common::delivery::progress::{DeliveryError, PreparedProgress, ProgressStore};
use std::{fmt, time::Duration};
use tokio::task::JoinHandle;
use tokio_postgres::{Client, NoTls};

/// The fixed `application_name`, so operators can find the sink's sessions.
pub const APPLICATION_NAME: &str = "drasi-native-storedproc-postgres";

pub(crate) type DriverResult = Result<(), tokio_postgres::Error>;
pub(crate) type Driver = JoinHandle<DriverResult>;

/// A user/password pair used for this activation: the configured one, or
/// host activation credentials that replace it.
#[derive(Clone)]
pub(crate) struct Login {
    pub user: String,
    pub password: String,
}

impl fmt::Debug for Login {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Login([REDACTED])")
    }
}

/// A failed connection attempt.
#[derive(Debug)]
pub struct ConnectError {
    permanent: bool,
    source: anyhow::Error,
}

impl ConnectError {
    fn permanent(source: impl Into<anyhow::Error>) -> Self {
        Self {
            permanent: true,
            source: source.into(),
        }
    }

    fn transient(source: impl Into<anyhow::Error>) -> Self {
        Self {
            permanent: false,
            source: source.into(),
        }
    }

    /// Whether another attempt can succeed without configuration changes.
    pub fn retryable(&self) -> bool {
        !self.permanent
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "connecting to PostgreSQL failed: {:#}", self.source)
    }
}

impl std::error::Error for ConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Classifies a connection failure by SQLSTATE: authentication, a missing
/// database and missing privileges are permanent; everything else (refused,
/// unreachable, starting up, too many connections, TLS I/O) is transient.
fn classify_connect(error: tokio_postgres::Error) -> ConnectError {
    match error.code().map(|code| code.code()) {
        Some(code) if code.starts_with("28") || code == "3D000" || code == "42501" => {
            ConnectError::permanent(error)
        }
        _ => ConnectError::transient(error),
    }
}

/// An open connection. Dropping it aborts the driver task; [`Self::close`]
/// and [`Self::discard`] hand the driver to the caller to join.
pub(crate) struct Connection {
    pub client: Client,
    driver: Option<Driver>,
    pub prepared: Option<PreparedProgress>,
    statement_timeout_ms: u64,
    /// Set while a delivery attempt uses the connection. Still set on entry
    /// means the host dropped the previous attempt mid-transaction, so the
    /// connection state is unknown and it is replaced.
    pub in_flight: bool,
}

impl Connection {
    /// One connection attempt, bounded by `connectTimeoutMs` and `deadline`.
    pub async fn connect(
        config: &PostgresStoredProcConfig,
        login: &Login,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Self, ConnectError> {
        let mut settings = tokio_postgres::Config::new();
        settings
            .host(&config.host)
            .port(config.port)
            .dbname(&config.database)
            .user(&login.user)
            .password(&login.password)
            .application_name(APPLICATION_NAME)
            .options(format!(
                "-c statement_timeout={}",
                config.command_timeout_ms
            ));
        let limit = bounded(config.connect_timeout(), deadline);
        let timed_out = || {
            ConnectError::transient(anyhow::anyhow!(
                "no connection within {limit:?} (connectTimeoutMs or the lifecycle deadline)"
            ))
        };
        let (client, driver): (Client, Driver) = match config.ssl_mode {
            SslMode::Require => {
                settings.ssl_mode(tokio_postgres::config::SslMode::Require);
                let mut tls = native_tls::TlsConnector::builder();
                if let Some(pem) = &config.tls_ca_pem {
                    tls.add_root_certificate(
                        native_tls::Certificate::from_pem(pem.as_bytes())
                            .context("invalid tlsCaPem")
                            .map_err(ConnectError::permanent)?,
                    );
                }
                if let (Some(certificate), Some(key)) = (
                    &config.tls_client_certificate_pem,
                    &config.tls_client_private_key_pem,
                ) {
                    tls.identity(
                        native_tls::Identity::from_pkcs8(certificate.as_bytes(), key.as_bytes())
                            .context("invalid TLS client certificate or key")
                            .map_err(ConnectError::permanent)?,
                    );
                }
                let connector = postgres_native_tls::MakeTlsConnector::new(
                    tls.build().map_err(ConnectError::permanent)?,
                );
                let (client, connection) = tokio::time::timeout(limit, settings.connect(connector))
                    .await
                    .map_err(|_| timed_out())?
                    .map_err(classify_connect)?;
                (client, tokio::spawn(drive(connection)))
            }
            SslMode::Disable => {
                settings.ssl_mode(tokio_postgres::config::SslMode::Disable);
                let (client, connection) = tokio::time::timeout(limit, settings.connect(NoTls))
                    .await
                    .map_err(|_| timed_out())?
                    .map_err(classify_connect)?;
                (client, tokio::spawn(drive(connection)))
            }
        };
        Ok(Self {
            client,
            driver: Some(driver),
            prepared: None,
            statement_timeout_ms: config.command_timeout_ms,
            in_flight: false,
        })
    }

    /// Whether the connection can carry another attempt.
    pub fn usable(&self) -> bool {
        !self.in_flight
            && !self.client.is_closed()
            && self
                .driver
                .as_ref()
                .is_some_and(|driver| !driver.is_finished())
    }

    /// Prepares the cursor statements once and applies an in-place change of
    /// `commandTimeoutMs` to this session.
    pub async fn ready(
        &mut self,
        store: &ProgressStore,
        config: &PostgresStoredProcConfig,
        limit: Duration,
    ) -> Result<(), DeliveryError> {
        if self.prepared.is_none() {
            self.prepared = Some(store.prepare(&self.client, Some(limit)).await?);
        }
        if self.statement_timeout_ms != config.command_timeout_ms {
            let statement = format!("SET statement_timeout = {}", config.command_timeout_ms);
            match tokio::time::timeout(limit, self.client.batch_execute(&statement)).await {
                Ok(result) => result.map_err(DeliveryError::Database)?,
                Err(_) => return Err(DeliveryError::Timeout(limit)),
            }
            self.statement_timeout_ms = config.command_timeout_ms;
        }
        Ok(())
    }

    /// Closes the client gracefully and returns the driver, which finishes
    /// once the server has answered any command still running.
    pub fn close(mut self) -> Option<Driver> {
        self.driver.take()
    }

    /// Drops the connection without waiting for the server and joins the
    /// aborted driver.
    pub async fn discard(self) {
        if let Some(driver) = self.close() {
            driver.abort();
            let _ = driver.await;
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.take() {
            driver.abort();
        }
    }
}

async fn drive(
    connection: impl std::future::Future<Output = DriverResult> + Send + 'static,
) -> DriverResult {
    let result = connection.await;
    if let Err(error) = &result {
        log::warn!("Native PostgreSQL sink connection ended: {error}");
    }
    result
}

/// `limit`, shortened to end no later than `deadline`.
pub(crate) fn bounded(limit: Duration, deadline: Option<tokio::time::Instant>) -> Duration {
    match deadline {
        Some(deadline) => {
            limit.min(deadline.saturating_duration_since(tokio::time::Instant::now()))
        }
        None => limit,
    }
}
