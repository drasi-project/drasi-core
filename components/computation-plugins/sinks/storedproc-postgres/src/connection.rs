// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Opens the activation's PostgreSQL sessions.

use crate::config::{PostgresStoredProcConfig, SslMode};
use anyhow::Context;
use async_trait::async_trait;
use drasi_computation_storedproc_common::driver::Connector;
pub use drasi_postgres_common::delivery::progress::ConnectError;
use drasi_postgres_common::delivery::progress::{Driver, PgSession};
use std::{fmt, time::Duration};
use tokio_postgres::{Client, NoTls};

/// The fixed `application_name`, so operators can find the sink's sessions.
pub const APPLICATION_NAME: &str = "drasi-native-storedproc-postgres";

pub(crate) type DriverResult = Result<(), tokio_postgres::Error>;

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

/// Opens sessions for one activation: the configuration's endpoint with
/// the activation's login, bound to this sink's owner and progress table.
pub(crate) struct PgConnector {
    config: PostgresStoredProcConfig,
    login: Login,
    owner: String,
}

impl PgConnector {
    pub(crate) fn new(config: PostgresStoredProcConfig, login: Login, owner: String) -> Self {
        Self {
            config,
            login,
            owner,
        }
    }

    /// Settings read per connection (in-place updates).
    pub(crate) fn set_config(&mut self, config: PostgresStoredProcConfig) {
        self.config = config;
    }

    /// One connection attempt, bounded by `connectTimeoutMs`.
    async fn open(&self) -> Result<(Client, Driver), ConnectError> {
        let config = &self.config;
        let mut settings = tokio_postgres::Config::new();
        settings
            .host(&config.host)
            .port(config.port)
            .dbname(&config.database)
            .user(&self.login.user)
            .password(&self.login.password)
            .application_name(APPLICATION_NAME)
            .options(format!(
                "-c statement_timeout={}",
                config.command_timeout_ms
            ));
        let limit = config.connect_timeout();
        let timed_out = || {
            ConnectError::transient(anyhow::anyhow!(
                "no connection within {limit:?} (connectTimeoutMs)"
            ))
        };
        Ok(match config.ssl_mode {
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
                    .map_err(ConnectError::classify)?;
                (client, tokio::spawn(drive(connection)))
            }
            SslMode::Disable => {
                settings.ssl_mode(tokio_postgres::config::SslMode::Disable);
                let (client, connection) = tokio::time::timeout(limit, settings.connect(NoTls))
                    .await
                    .map_err(|_| timed_out())?
                    .map_err(ConnectError::classify)?;
                (client, tokio::spawn(drive(connection)))
            }
        })
    }
}

#[async_trait]
impl Connector for PgConnector {
    type Session = PgSession;

    async fn connect(&mut self) -> anyhow::Result<PgSession> {
        let (client, driver) = self.open().await?;
        PgSession::new(
            client,
            driver,
            self.owner.clone(),
            self.config.table()?,
            self.config.client_command_timeout(),
        )
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
