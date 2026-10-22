// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::PostgresStoredProcConfig;
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    workers::{join_owned_worker, spawn_owned_worker, WorkerCompletion},
    ComponentDescriptor, ComponentId, DeliveryHandler, DeliveryItem, PipeRequirements,
    PortDescriptor, PortDirection, PortId, QueryChangeCodec,
};
use drasi_computation_plugin_sdk::NativeComponent;
use drasi_postgres_common::{
    delivery::{PostgresDeliveryEffect, PostgresDeliveryHandler},
    sql::{build_native_render_input, execute_parameters, CompiledTemplate},
};
use drasi_reaction_common::TemplateRouting;
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::{sync::RwLock, task::JoinHandle};
use tokio_postgres::{Client, NoTls, Transaction};

type DriverResult = std::result::Result<(), tokio_postgres::Error>;
type Driver = Pin<Box<dyn Future<Output = DriverResult> + Send>>;

fn is_loopback_hostname(hostname: &str) -> bool {
    hostname.eq_ignore_ascii_case("localhost")
        || hostname
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

struct ProcedureEffect {
    config: Arc<PostgresStoredProcConfig>,
    templates: Arc<HashMap<String, CompiledTemplate>>,
}

#[async_trait]
impl PostgresDeliveryEffect for ProcedureEffect {
    async fn apply(&mut self, transaction: &Transaction<'_>, item: DeliveryItem<'_>) -> Result<()> {
        let input = build_native_render_input(item.envelope, item.operation)?;
        anyhow::ensure!(
            self.config.queries.contains(&input.query_id),
            "PostgreSQL sink received unconfigured query '{}'",
            input.query_id
        );
        let Some(spec) = self
            .config
            .get_template_spec(&input.query_id, input.operation)
            .filter(|spec| !spec.template.is_empty())
        else {
            log::debug!(
                "PostgreSQL sink: no {:?} command configured for query '{}'",
                input.operation,
                input.query_id
            );
            return Ok(());
        };
        let template = self
            .templates
            .get(&spec.template)
            .context("missing compiled PostgreSQL command")?;
        let (command, parameters) = template.render(&input.context)?;
        if command.trim().is_empty() {
            log::debug!(
                "PostgreSQL sink: command condition excluded query '{}'",
                input.query_id
            );
            return Ok(());
        }
        execute_parameters(transaction, &command, parameters).await?;
        Ok(())
    }
}

/// A native external consumer: the host owns delivery progress/retry, while
/// PostgreSQL owns the atomic business effect and bounded destination cursor.
pub struct PostgresStoredProcSink {
    descriptor: ComponentDescriptor,
    config: Arc<PostgresStoredProcConfig>,
    templates: Arc<HashMap<String, CompiledTemplate>>,
    handler: Option<PostgresDeliveryHandler<ProcedureEffect>>,
    driver: RwLock<Option<JoinHandle<DriverResult>>>,
    cleanup_required: bool,
    interrupted: bool,
}

impl PostgresStoredProcSink {
    pub fn new(id: ComponentId, config: PostgresStoredProcConfig) -> Result<Self> {
        config.validate()?;
        let templates = Arc::new(config.compile_templates()?);
        Ok(Self {
            descriptor: Self::describe(id)?,
            config: Arc::new(config),
            templates,
            handler: None,
            driver: RwLock::new(None),
            cleanup_required: false,
            interrupted: false,
        })
    }

    pub fn describe(id: ComponentId) -> Result<ComponentDescriptor> {
        Ok(ComponentDescriptor::try_new(
            id,
            vec![PortDescriptor::new(
                PortId::try_new("in")?,
                PortDirection::Input,
                QueryChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?)
    }

    async fn connect(&self) -> Result<(Client, Driver)> {
        let mut config = tokio_postgres::Config::new();
        config
            .host(&self.config.hostname)
            .port(self.config.port)
            .dbname(&self.config.database)
            .user(&self.config.user)
            .password(&self.config.password)
            .application_name("drasi-native-storedproc-postgres")
            .connect_timeout(Duration::from_millis(self.config.connect_timeout_ms.get()))
            .options(format!(
                "-c statement_timeout={}",
                self.config.command_timeout_ms
            ));
        if self.config.ssl {
            config.ssl_mode(tokio_postgres::config::SslMode::Require);
            log::debug!(
                "Native PostgreSQL sink verifies TLS using system trust{}",
                if self.config.tls_ca_pem.is_some() {
                    " and the configured additional CA"
                } else {
                    ""
                }
            );
            let mut tls = native_tls::TlsConnector::builder();
            if let Some(pem) = &self.config.tls_ca_pem {
                tls.add_root_certificate(
                    native_tls::Certificate::from_pem(pem.as_bytes())
                        .context("invalid PostgreSQL CA certificate")?,
                );
            }
            if let (Some(cert), Some(key)) = (
                &self.config.tls_client_cert_pem,
                &self.config.tls_client_key_pem,
            ) {
                tls.identity(
                    native_tls::Identity::from_pkcs8(cert.as_bytes(), key.as_bytes())
                        .context("invalid PostgreSQL client certificate or key")?,
                );
            }
            let (client, connection) = config
                .connect(postgres_native_tls::MakeTlsConnector::new(tls.build()?))
                .await?;
            Ok((client, Box::pin(connection)))
        } else {
            if !is_loopback_hostname(&self.config.hostname) {
                log::warn!(
                    "Native PostgreSQL sink connects to a non-loopback host without TLS; \
                     query data and authentication traffic are not encrypted. Set ssl=true."
                );
            }
            config.ssl_mode(tokio_postgres::config::SslMode::Disable);
            let (client, connection) = config.connect(NoTls).await?;
            Ok((client, Box::pin(connection)))
        }
    }
}

#[async_trait]
impl NativeComponent for PostgresStoredProcSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({"settings": self.config.as_ref()}))
    }
    async fn start(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.cleanup_required,
            "PostgreSQL sink requires awaited cleanup before restart"
        );
        self.cleanup_required = true;
        let (client, connection) = tokio::time::timeout(
            Duration::from_millis(self.config.connect_timeout_ms.get()),
            self.connect(),
        )
        .await
        .context("PostgreSQL sink connection timed out")??;
        self.handler = Some(PostgresDeliveryHandler::new(
            client,
            ProcedureEffect {
                config: self.config.clone(),
                templates: self.templates.clone(),
            },
        ));
        spawn_owned_worker(&self.driver, async move {
            let result = connection.await;
            if let Err(error) = &result {
                log::error!("Native PostgreSQL delivery connection failed: {error}");
            }
            result
        })
        .await?;
        tokio::time::timeout(
            Duration::from_millis(self.config.command_timeout_ms.get()),
            self.handler
                .as_mut()
                .context("PostgreSQL handler missing")?
                .initialize(),
        )
        .await
        .context("PostgreSQL delivery initialization timed out")??;
        self.interrupted = false;
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        self.handler = None;
        match join_owned_worker(
            self.driver.get_mut(),
            Duration::from_millis(self.config.shutdown_timeout_ms.get()),
        )
        .await?
        {
            WorkerCompletion::Completed(result) => {
                result.context("PostgreSQL connection driver failed")?
            }
            WorkerCompletion::Absent | WorkerCompletion::Cancelled => {}
        }
        self.cleanup_required = false;
        self.interrupted = false;
        Ok(())
    }
}

#[async_trait]
impl DeliveryHandler for PostgresStoredProcSink {
    fn retryable(&self, error: &anyhow::Error) -> bool {
        !self.interrupted
            && self
                .handler
                .as_ref()
                .is_some_and(|handler| handler.retryable(error))
    }
    async fn handle(&mut self, item: DeliveryItem<'_>) -> Result<()> {
        anyhow::ensure!(
            !self.interrupted,
            "PostgreSQL delivery requires cleanup after interruption"
        );
        anyhow::ensure!(
            item.port.as_str() == "in",
            "unknown PostgreSQL sink input port"
        );
        let handler = self
            .handler
            .as_mut()
            .context("PostgreSQL sink is not running")?;
        self.interrupted = true;
        let result = tokio::time::timeout(
            Duration::from_millis(self.config.command_timeout_ms.get()),
            handler.handle(item),
        )
        .await
        .context("PostgreSQL delivery timed out; outcome requires recovery")?;
        if result.is_ok() || result.as_ref().is_err_and(|error| handler.retryable(error)) {
            self.interrupted = false;
        }
        result
    }
}

impl Drop for PostgresStoredProcSink {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.get_mut() {
            driver.abort();
            log::warn!("Native PostgreSQL sink dropped without awaited connection cleanup");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_loopback_hostname;

    #[test]
    fn plaintext_warning_only_exempts_explicit_loopback_hosts() {
        for hostname in ["localhost", "LOCALHOST", "127.0.0.1", "127.0.0.2", "::1"] {
            assert!(is_loopback_hostname(hostname), "{hostname}");
        }
        for hostname in ["0.0.0.0", "::", "database.example", "localhost.example"] {
            assert!(!is_loopback_hostname(hostname), "{hostname}");
        }
    }
}
