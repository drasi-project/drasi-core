// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use drasi_computation_contracts::{OutputAliases, SinkOutputs};
use drasi_postgres_common::sql::{resolve_template, CompiledTemplate};
use drasi_reaction_common::{OperationType, QueryConfig, TemplateRouting, TemplateSpec};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, num::NonZeroU64, time::Duration};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostgresStoredProcConfig {
    #[serde(default = "default_hostname")]
    pub hostname: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub database: String,
    pub user: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub ssl: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_cert_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_key_pem: Option<String>,
    /// Optional aliases for the incoming outputs; the graph supplies each
    /// output's producing component, port and stream.
    #[serde(default, skip_serializing_if = "OutputAliases::is_empty")]
    pub outputs: OutputAliases,
    #[serde(default)]
    pub routes: HashMap<String, QueryConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig>,
    #[serde(default = "default_timeout")]
    pub command_timeout_ms: NonZeroU64,
    #[serde(default = "default_timeout")]
    pub connect_timeout_ms: NonZeroU64,
    #[serde(default = "default_timeout")]
    pub shutdown_timeout_ms: NonZeroU64,
}

fn default_hostname() -> String {
    "localhost".into()
}
fn default_port() -> u16 {
    5432
}
fn default_timeout() -> NonZeroU64 {
    NonZeroU64::new(30_000).expect("positive timeout")
}

impl PostgresStoredProcConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.port > 0
                && !self.hostname.is_empty()
                && !self.database.is_empty()
                && !self.user.is_empty(),
            "PostgreSQL hostname, port, database and user are required"
        );
        anyhow::ensure!(
            [&self.hostname, &self.database, &self.user, &self.password]
                .into_iter()
                .all(|value| !value.contains('\0')),
            "PostgreSQL connection fields cannot contain NUL"
        );
        anyhow::ensure!(
            self.command_timeout_ms.get() <= i32::MAX as u64,
            "PostgreSQL command timeout exceeds the server statement_timeout limit"
        );
        for timeout in [
            self.command_timeout_ms,
            self.connect_timeout_ms,
            self.shutdown_timeout_ms,
        ] {
            anyhow::ensure!(
                tokio::time::Instant::now()
                    .checked_add(Duration::from_millis(timeout.get()))
                    .is_some(),
                "PostgreSQL timeout is not representable"
            );
        }
        anyhow::ensure!(
            self.tls_client_cert_pem.is_some() == self.tls_client_key_pem.is_some(),
            "PostgreSQL client certificate and key must be supplied together"
        );
        for value in [
            &self.tls_ca_pem,
            &self.tls_client_cert_pem,
            &self.tls_client_key_pem,
        ]
        .into_iter()
        .flatten()
        {
            anyhow::ensure!(
                self.ssl && !value.is_empty() && value.len() <= 64 * 1024 && !value.contains('\0'),
                "PostgreSQL TLS material requires SSL and 1..=65536 bytes without NUL"
            );
        }
        self.outputs.validate()?;
        anyhow::ensure!(
            !self.compile_templates()?.is_empty(),
            "PostgreSQL sink requires at least one nonempty stored-procedure template"
        );
        Ok(())
    }

    pub(crate) fn compile_templates(&self) -> Result<HashMap<String, CompiledTemplate>> {
        let mut templates = HashMap::new();
        for config in self.routes.values().chain(self.default_template.iter()) {
            for spec in [&config.added, &config.updated, &config.deleted]
                .into_iter()
                .flatten()
            {
                if !spec.template.is_empty() && !templates.contains_key(&spec.template) {
                    templates.insert(
                        spec.template.clone(),
                        CompiledTemplate::compile(&spec.template)
                            .context("invalid PostgreSQL command template")?,
                    );
                }
            }
        }
        Ok(templates)
    }
}

impl PostgresStoredProcConfig {
    /// Checks that depend on the incoming edges: every route must name an
    /// output of this sink.
    pub fn validate_outputs(&self, outputs: &SinkOutputs) -> Result<()> {
        for key in self.routes.keys() {
            anyhow::ensure!(
                outputs
                    .aliases()
                    .any(|alias| alias == key || alias.rsplit('.').next() == Some(key.as_str())),
                "route '{key}' does not match an output alias or its last dotted segment"
            );
        }
        Ok(())
    }
}

impl TemplateRouting for PostgresStoredProcConfig {
    fn routes(&self) -> &HashMap<String, QueryConfig> {
        &self.routes
    }
    fn default_template(&self) -> Option<&QueryConfig> {
        self.default_template.as_ref()
    }
    fn get_template_spec(&self, query: &str, operation: OperationType) -> Option<&TemplateSpec> {
        resolve_template(
            &self.routes,
            self.default_template.as_ref(),
            query,
            operation,
        )
    }
}
