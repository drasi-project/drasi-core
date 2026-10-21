// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, ComponentRole, ImplementationIdentity, InputEnvelope,
    PipeRequirements, PortDescriptor, PortDirection, PortId, QueryChangeCodec, SinkCompletion,
};
use drasi_computation_plugin_sdk::{
    ActivationCredentialContext, ActivationCredentials, Capabilities, Component, ConfigSchema,
    ConfigType, ControlSender, CreateRequest, CreatedComponent, Factory, FactoryMetadata,
    NativeComponent, NativeSink, PluginDefinition,
};
use drasi_mssql_common::{
    native::{NativeConfig, Work},
    sql, MsSqlConnection, MsSqlSourceConfig,
};
pub use drasi_reaction_common::{QueryConfig, TemplateSpec};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
    time::Duration,
};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const MSSQL_SINK: &str = "drasi.mssql/storedproc";
pub struct MsSqlStoredProcFactory;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MsSqlStoredProcConfig {
    pub connection: MsSqlSourceConfig,
    pub queries: Vec<String>,
    #[serde(default)]
    pub routes: HashMap<String, QueryConfig>,
    #[serde(default)]
    pub default_template: Option<QueryConfig>,
    #[serde(default = "default_timeout")]
    pub command_timeout_ms: u64,
    #[serde(default = "default_timeout")]
    pub shutdown_timeout_ms: u64,
}
fn default_timeout() -> u64 {
    30_000
}
impl MsSqlStoredProcConfig {
    pub fn validate(&self) -> Result<()> {
        self.validate_with_binding(false)
    }
    fn validate_with_binding(&self, bound: bool) -> Result<()> {
        NativeConfig {
            connection: self.connection.clone(),
            barrier_table: None,
            max_rows: 1,
            max_bytes: 1024,
            operation_timeout_ms: self.command_timeout_ms,
            shutdown_timeout_ms: self.shutdown_timeout_ms,
        }
        .validate_with_bound_credentials(bound)?;
        ensure!(bound || !self.connection.password.is_empty(),
            "SQL Server sink requires literal credentials unless an actual activation identity is bound");
        ensure!(
            !self.queries.is_empty() && self.queries.len() <= 256,
            "SQL Server sink requires 1..=256 query IDs"
        );
        let mut queries = BTreeSet::new();
        for query in &self.queries {
            ComponentId::try_new(query.as_str())?;
            ensure!(queries.insert(query), "duplicate SQL Server sink query");
        }
        for route in self.routes.keys() {
            ensure!(
                queries.contains(route)
                    || self
                        .queries
                        .iter()
                        .any(|id| id.rsplit('.').next() == Some(route.as_str())),
                "SQL Server route does not match a configured query"
            );
        }
        for config in self.routes.values().chain(self.default_template.iter()) {
            for template in [&config.added, &config.updated, &config.deleted]
                .into_iter()
                .flatten()
            {
                sql::validate_template(&template.template)?;
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    settings: MsSqlStoredProcConfig,
}

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(MsSqlStoredProcFactory)],
        vec![QueryChangeCodec::schema()],
    )
}
impl Factory for MsSqlStoredProcFactory {
    fn supports_activation_credentials(&self) -> bool {
        true
    }
    fn activation_credential_context(
        &self,
        request: &CreateRequest,
    ) -> Result<ActivationCredentialContext> {
        let config: Configuration = serde_json::from_value(request.configuration.clone())?;
        let context = ActivationCredentialContext {
            hostname: config.settings.connection.host,
            port: config.settings.connection.port,
        };
        context.validate()?;
        Ok(context)
    }
    fn metadata(&self) -> FactoryMetadata {
        let mut configuration = ConfigSchema::from_fields([("settings", ConfigType::Object, true)]);
        configuration
            .fields
            .get_mut("settings")
            .expect("settings")
            .secret = true;
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(MSSQL_SINK, "1")
                .expect("implementation"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration,
            ports: MsSqlStoredProcSink::describe(ComponentId::try_new("mssql").expect("id"))
                .expect("descriptor")
                .ports()
                .to_vec(),
            completion: Some(SinkCompletion::Handled),
            capabilities: Capabilities::default(),
        }
    }
    fn create(&self, request: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config: Configuration = serde_json::from_value(request.configuration.clone())?;
        Ok(
            Component::Sink(Box::new(MsSqlStoredProcSink::new_with_binding(
                request.id.clone(),
                config.settings,
                request.activation_credentials_bound,
            )?))
            .into(),
        )
    }
}

pub struct MsSqlStoredProcSink {
    descriptor: ComponentDescriptor,
    config: MsSqlStoredProcConfig,
    work: Work<()>,
    cleanup: bool,
    interrupted: bool,
    identity_bound: bool,
    active_connection: Option<MsSqlSourceConfig>,
}
impl MsSqlStoredProcSink {
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
    pub fn new(id: ComponentId, config: MsSqlStoredProcConfig) -> Result<Self> {
        Self::new_with_binding(id, config, false)
    }
    fn new_with_binding(
        id: ComponentId,
        config: MsSqlStoredProcConfig,
        identity_bound: bool,
    ) -> Result<Self> {
        config.validate_with_binding(identity_bound)?;
        Ok(Self {
            descriptor: Self::describe(id)?,
            config,
            work: Work::default(),
            cleanup: false,
            interrupted: false,
            identity_bound,
            active_connection: None,
        })
    }
    fn authenticated_connection(
        &self,
        credentials: ActivationCredentials,
    ) -> Result<MsSqlSourceConfig> {
        ensure!(
            self.identity_bound,
            "SQL Server activation credentials were not bound by the host"
        );
        credentials.validate()?;
        let (username, password) = credentials.into_auth_pair();
        let mut connection = self.config.connection.clone();
        connection.user = username;
        connection.password = password;
        // A token follows the legacy provider -> SQL-auth pair behavior. It is
        // not an assertion that SQL Server's Azure AD token protocol is supported.
        NativeConfig {
            connection: connection.clone(),
            barrier_table: None,
            max_rows: 1,
            max_bytes: 1024,
            operation_timeout_ms: self.config.command_timeout_ms,
            shutdown_timeout_ms: self.config.shutdown_timeout_ms,
        }
        .validate()?;
        Ok(connection)
    }
    async fn activate(&mut self, connection: MsSqlSourceConfig) -> Result<()> {
        ensure!(
            !self.cleanup,
            "SQL Server sink requires successful stop before restart"
        );
        self.cleanup = true;
        self.interrupted = true;
        self.active_connection = Some(connection.clone());
        self.work.start(async move {
            MsSqlConnection::connect(&connection)
                .await?
                .test_connection()
                .await
        })?;
        self.work
            .wait(Duration::from_millis(self.config.command_timeout_ms))
            .await?;
        self.interrupted = false;
        Ok(())
    }
}
#[async_trait]
impl NativeComponent for MsSqlStoredProcSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({"settings":self.config}))
    }
    async fn start(&mut self) -> Result<()> {
        ensure!(
            !self.identity_bound,
            "SQL Server provider-bound activation cannot fall back to literal credentials"
        );
        self.activate(self.config.connection.clone()).await
    }
    async fn start_with_credentials(&mut self, credentials: ActivationCredentials) -> Result<()> {
        let connection = self.authenticated_connection(credentials)?;
        self.activate(connection).await
    }
    async fn stop(&mut self) -> Result<()> {
        self.work
            .stop(Duration::from_millis(self.config.shutdown_timeout_ms))
            .await?;
        self.cleanup = false;
        self.interrupted = false;
        self.active_connection = None;
        Ok(())
    }
}
#[async_trait]
impl NativeSink for MsSqlStoredProcSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        ensure!(
            self.cleanup && !self.interrupted && !self.work.pending(),
            "SQL Server sink is stopped or requires cleanup after interrupted work"
        );
        ensure!(input.port.as_str() == "in", "unknown SQL Server sink port");
        ensure!(
            !QueryChangeCodec::is_snapshot(&input.envelope),
            "SQL Server sink does not implement snapshot replacement"
        );
        let result = QueryChangeCodec::to_legacy_result(&input.envelope)?;
        ensure!(
            self.config.queries.contains(&result.query_id),
            "SQL Server sink received an unconfigured query"
        );
        let mut commands = Vec::new();
        for diff in &result.results {
            if let Some((operation, context)) = sql::build_context(&result, diff) {
                if let Some(template) = sql::resolve_template(
                    &self.config.routes,
                    self.config.default_template.as_ref(),
                    &result.query_id,
                    operation,
                ) {
                    let (command, parameters) = sql::render_command(&template.template, &context)?;
                    if !command.trim().is_empty() {
                        commands.push((command, parameters));
                    }
                }
            }
        }
        if commands.is_empty() {
            return Ok(());
        }
        ensure!(
            commands.len() <= 100_000,
            "SQL Server input exceeds command limit"
        );
        let connection = self
            .active_connection
            .clone()
            .context("SQL Server activation credentials are not available")?;
        self.interrupted = true;
        self.work.start(async move {
            let mut connection = MsSqlConnection::connect(&connection).await?;
            for (command, parameters) in commands {
                let mut query = tiberius::Query::new(command);
                sql::bind_parameters(&mut query, &parameters);
                let mut results = query.query(connection.client_mut()).await?;
                while results.try_next().await?.is_some() {}
            }
            Ok(())
        })?;
        self.work
            .wait(Duration::from_millis(self.config.command_timeout_ms))
            .await?;
        self.interrupted = false;
        Ok(())
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> serde_json::Value {
        serde_json::json!({"connection":{"host":"no-network-at-construction","database":"d","user":"u","password":"p"},
            "queries":["source.q"],"default_template":{"added":{"template":"EXEC p {{param after.id}}"}}})
    }
    #[test]
    fn real_sink_factory_and_configuration_preserve_secrets() -> Result<()> {
        drasi_computation_plugin_sdk::ExportedPlugin::new(plugin()?)?;
        let sink = MsSqlStoredProcSink::new(
            ComponentId::try_new("sink")?,
            serde_json::from_value(config())?,
        )?;
        assert_eq!(
            sink.configuration()?["settings"]["connection"]["password"],
            "p"
        );
        assert_eq!(sink.completion(), SinkCompletion::Handled);
        Ok(())
    }
    #[test]
    fn unsafe_and_unsupported_options_are_rejected() {
        for (field, value) in [
            ("queries", serde_json::json!([])),
            ("command_timeout_ms", serde_json::json!(0)),
            ("retry_attempts", serde_json::json!(3)),
            ("identity_provider", serde_json::json!("fake")),
        ] {
            let mut config = config();
            config[field] = value;
            assert!(
                serde_json::from_value::<MsSqlStoredProcConfig>(config)
                    .map_err(anyhow::Error::from)
                    .and_then(|c| c.validate())
                    .is_err(),
                "{field}"
            );
        }
    }

    #[tokio::test]
    async fn activation_binding_controls_empty_literals_and_never_mutates_declared_config(
    ) -> Result<()> {
        let mut empty = config();
        empty["connection"]["user"] = serde_json::json!("");
        empty["connection"]["password"] = serde_json::json!("");
        let empty: MsSqlStoredProcConfig = serde_json::from_value(empty)?;
        assert!(MsSqlStoredProcSink::new(ComponentId::try_new("sink")?, empty.clone()).is_err());
        let mut sink =
            MsSqlStoredProcSink::new_with_binding(ComponentId::try_new("sink")?, empty, true)?;
        let declared = sink.configuration()?;
        assert!(sink.start().await.is_err());
        for credentials in [
            ActivationCredentials::UsernamePassword {
                username: "provider-user".into(),
                password: "first-secret".into(),
            },
            ActivationCredentials::Token {
                username: "provider-user".into(),
                token: "refreshed-secret".into(),
            },
        ] {
            let expected = credentials.clone().into_auth_pair();
            let connection = sink.authenticated_connection(credentials)?;
            assert_eq!((connection.user, connection.password), expected);
            assert_eq!(
                connection.auth_mode,
                drasi_mssql_common::AuthMode::SqlServer
            );
            assert_eq!(sink.configuration()?, declared);
        }
        assert!(sink
            .start_with_credentials(ActivationCredentials::UsernamePassword {
                username: String::new(),
                password: "must-not-fall-back".into(),
            })
            .await
            .is_err());
        assert!(sink.active_connection.is_none());
        assert_eq!(sink.configuration()?, declared);
        sink.stop().await?;
        Ok(())
    }

    #[test]
    fn actual_provider_values_override_valid_literals_and_endpoint_is_configured_only() -> Result<()>
    {
        let sink = MsSqlStoredProcSink::new_with_binding(
            ComponentId::try_new("sink")?,
            serde_json::from_value(config())?,
            true,
        )?;
        let declared = sink.configuration()?;
        let connection =
            sink.authenticated_connection(ActivationCredentials::UsernamePassword {
                username: "injected".into(),
                password: "activation-only".into(),
            })?;
        assert_eq!(connection.user, "injected");
        assert_eq!(connection.password, "activation-only");
        assert_eq!(sink.configuration()?, declared);
        let request = CreateRequest {
            id: ComponentId::try_new("sink")?,
            implementation: MsSqlStoredProcFactory.metadata().implementation,
            configuration_version: 1,
            configuration: serde_json::json!({"settings":config()}),
            scope: None,
            activation_credentials_bound: true,
        };
        let context = MsSqlStoredProcFactory.activation_credential_context(&request)?;
        assert_eq!(context.hostname, "no-network-at-construction");
        assert_eq!(context.port, 1433);
        assert!(MsSqlStoredProcFactory.supports_activation_credentials());
        Ok(())
    }
}
