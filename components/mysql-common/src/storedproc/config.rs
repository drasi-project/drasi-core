// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{
    native::{MySqlConnectionConfig, MySqlTlsMode},
    storedproc,
};
use anyhow::Result;
use drasi_computation_contracts::ComponentId;
use drasi_reaction_common::{OperationType, QueryConfig};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
};

fn host() -> String {
    "localhost".into()
}
fn port() -> u16 {
    3306
}
fn command_timeout() -> NonZeroU64 {
    NonZeroU64::new(30_000).expect("nonzero command timeout")
}
fn connect_timeout() -> NonZeroU64 {
    NonZeroU64::new(5_000).expect("nonzero connection timeout")
}
fn shutdown_timeout() -> NonZeroU64 {
    NonZeroU64::new(10_000).expect("nonzero shutdown timeout")
}
fn protocol_bytes() -> NonZeroUsize {
    NonZeroUsize::new(8 << 20).expect("nonzero protocol limit")
}
fn retries() -> u32 {
    3
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MySqlStoredProcConfig {
    #[serde(default = "host")]
    pub hostname: String,
    #[serde(default = "port")]
    pub port: u16,
    pub database: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub ssl: bool,
    #[serde(default)]
    pub tls_ca_pem: Option<String>,
    pub queries: Vec<String>,
    #[serde(default)]
    pub routes: HashMap<String, QueryConfig>,
    #[serde(default)]
    pub default_template: Option<QueryConfig>,
    #[serde(default = "command_timeout")]
    pub command_timeout_ms: NonZeroU64,
    #[serde(default = "connect_timeout")]
    pub connect_timeout_ms: NonZeroU64,
    #[serde(default = "shutdown_timeout")]
    pub shutdown_timeout_ms: NonZeroU64,
    #[serde(default = "protocol_bytes")]
    pub max_protocol_bytes: NonZeroUsize,
    #[serde(default = "retries")]
    pub retry_attempts: u32,
}

impl MySqlStoredProcConfig {
    pub fn connection(&self) -> MySqlConnectionConfig {
        MySqlConnectionConfig {
            host: self.hostname.clone(),
            port: self.port,
            database: self.database.clone(),
            user: self.user.clone(),
            password: self.password.clone(),
            ssl_mode: if self.ssl {
                MySqlTlsMode::RequireVerifyFull
            } else {
                MySqlTlsMode::Disabled
            },
            tls_ca_pem: self.tls_ca_pem.clone(),
            server_id: NonZeroU32::new(1).expect("nonzero unused SQL connection server id"),
            heartbeat_interval_ms: NonZeroU64::new(1000).expect("nonzero unused SQL heartbeat"),
        }
    }
    pub fn validate(&self) -> Result<()> {
        self.validate_with_activation_credentials(false)
    }
    pub fn validate_with_activation_credentials(&self, credentials_bound: bool) -> Result<()> {
        self.connection().validate_with_activation_credentials(
            self.max_protocol_bytes.get(),
            credentials_bound,
        )?;
        anyhow::ensure!(
            self.max_protocol_bytes.get() <= 16 * 1024 * 1024 && self.retry_attempts <= 8,
            "MySQL command limits exceed supported bounds"
        );
        anyhow::ensure!(
            self.ssl || self.tls_ca_pem.is_none(),
            "MySQL CA requires TLS"
        );
        anyhow::ensure!(
            !self.queries.is_empty()
                && self.queries.iter().collect::<BTreeSet<_>>().len() == self.queries.len(),
            "MySQL sink requires distinct configured queries"
        );
        for query in &self.queries {
            ComponentId::try_new(query.as_str())?;
        }
        storedproc::validate_routes(&self.routes, &self.queries)?;
        anyhow::ensure!(
            !self.routes.is_empty() || self.default_template.is_some(),
            "MySQL sink needs a command template"
        );
        for query in self.routes.values().chain(self.default_template.iter()) {
            for template in [
                query.added.as_ref(),
                query.updated.as_ref(),
                query.deleted.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                storedproc::validate_parameterized_template(&template.template)?;
            }
        }
        for timeout in [
            self.command_timeout_ms,
            self.connect_timeout_ms,
            self.shutdown_timeout_ms,
        ] {
            anyhow::ensure!(
                timeout.get() <= 86_400_000,
                "MySQL operation timeout exceeds one day"
            );
        }
        Ok(())
    }
    pub fn template(&self, query: &str, operation: OperationType) -> Option<&str> {
        storedproc::resolve_template(
            &self.routes,
            self.default_template.as_ref(),
            query,
            operation,
        )
        .map(|template| template.template.as_str())
    }
}
