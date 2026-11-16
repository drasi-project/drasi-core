// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native SQL Server CDC source (`drasi.mssql/changes`).
//!
//! One worker per activation polls the selected tables' CDC change tables
//! over one reused connection, in bounded pages, and emits one envelope per
//! row change to a fixed set of consumers with per-consumer replay. It
//! pauses at page boundaries, reconnects after transport failures, fails
//! with typed errors on schema or retention problems, supports managed
//! updates, and deletes its own orphaned snapshot markers on erasing
//! removal.
//!
//! See `DESIGN.md` and `CONFIGURATION.md` in this crate. Live SQL Server
//! qualification is deferred (CW-78).

mod component;
pub mod config;
mod management;
mod progress;
mod worker;

use anyhow::{Context, Result};
use config::SourceConfig;
use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateMode, GraphChangeCodec, ImplementationIdentity,
};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, ActivationCredentialContext, Capabilities, Component, ConfigDetails,
    ControlSender, CreateRequest, CreatedComponent, Factory, FactoryMetadata,
    NativeConsumerProgress, PluginDefinition, UpdateRequest,
};
use std::sync::Arc;

pub use component::{tiberius_connectors, ConnectorFactory, MsSqlSource};
pub use drasi_mssql_common::native::NativeSettings;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const MSSQL_SOURCE: &str = "drasi.mssql/changes";

/// Factory for `drasi.mssql/changes`.
pub struct MsSqlSourceFactory;

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(MsSqlSourceFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

impl MsSqlSourceFactory {
    /// Creates the source with `connectors` (tests inject an in-memory
    /// database; the factory uses Tiberius).
    pub fn create_source(
        &self,
        request: &CreateRequest,
        progress: &NativeConsumerProgress,
        connectors: ConnectorFactory,
    ) -> Result<MsSqlSource> {
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config = SourceConfig::from_value(&request.configuration)?;
        let scope = request
            .scope
            .as_ref()
            .context("SQL Server source requires graph scope")?;
        let reader = progress.reader();
        anyhow::ensure!(
            reader.graph_id() == scope.graph_id && reader.producer_id() == &request.id,
            "SQL Server consumer progress belongs to another graph or producer"
        );
        MsSqlSource::new(
            request.id.clone(),
            config,
            reader,
            progress.consumer_ids(),
            request.activation_credentials_bound,
            connectors,
        )
    }
}

impl Factory for MsSqlSourceFactory {
    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            // Constant identifiers; validated by `ExportedPlugin::new`.
            implementation: ImplementationIdentity::try_new(MSSQL_SOURCE, "1")
                .expect("constant implementation"),
            role: ComponentRole::Source,
            configuration_version: 1,
            configuration: config::definition().schema(),
            ports: MsSqlSource::ports().expect("constant port descriptor"),
            completion: None,
            capabilities: Capabilities {
                management: true,
                update_modes: vec![ComponentUpdateMode::InPlace, ComponentUpdateMode::Restart],
                erases_owned_state: true,
                ..Capabilities::default()
            },
        }
    }
    fn configuration_details(&self) -> Option<ConfigDetails> {
        Some(config::definition().details())
    }
    fn supports_update_planning(&self) -> bool {
        true
    }
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        management::assess(request)
    }
    fn supports_activation_credentials(&self) -> bool {
        true
    }
    fn activation_credential_context(
        &self,
        request: &CreateRequest,
    ) -> Result<ActivationCredentialContext> {
        let config = SourceConfig::from_value(&request.configuration)?;
        let context = ActivationCredentialContext {
            hostname: config.settings.host,
            port: config.settings.port,
        };
        context.validate()?;
        Ok(context)
    }
    fn supports_consumer_progress(&self) -> bool {
        true
    }
    fn create(&self, _: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        anyhow::bail!("SQL Server source requires its actual consumer-progress binding")
    }
    fn create_with_consumer_progress(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        progress: NativeConsumerProgress,
    ) -> Result<CreatedComponent> {
        Ok(Component::Source(Box::new(self.create_source(
            request,
            &progress,
            tiberius_connectors(),
        )?))
        .into())
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_metadata_constructs_without_database_io() -> Result<()> {
        drasi_computation_plugin_sdk::ExportedPlugin::new(plugin()?)?;
        #[cfg(feature = "dynamic-plugin")]
        assert!(!drasi_computation_plugin_metadata().is_null());
        let metadata = MsSqlSourceFactory.metadata();
        assert_eq!(metadata.role, ComponentRole::Source);
        assert_eq!(metadata.completion, None);
        assert_eq!(metadata.ports, MsSqlSource::ports()?);
        assert_eq!(metadata.ports.len(), 1);
        assert_eq!(metadata.ports[0].id().as_str(), "out");
        assert_eq!(
            metadata.ports[0].schema(),
            GraphChangeCodec::schema().descriptor()
        );
        assert!(metadata.capabilities.management && metadata.capabilities.erases_owned_state);
        assert!(metadata.configuration.fields["password"].secret);
        assert!(!metadata.configuration.fields.contains_key("settings"));
        assert!(MsSqlSourceFactory.supports_consumer_progress());
        assert!(MsSqlSourceFactory.supports_activation_credentials());
        let details = MsSqlSourceFactory
            .configuration_details()
            .context("configuration details")?;
        details.validate_schema(&metadata.configuration)?;
        Ok(())
    }

    #[test]
    fn activation_context_names_the_configured_endpoint() -> Result<()> {
        let request = drasi_computation_plugin_sdk::testing::create_request(
            &MsSqlSourceFactory,
            &drasi_computation_contracts::ComponentId::generate(),
            serde_json::json!({"stream": "s/out", "host": "sql.corp", "port": 14330, "database": "d"}),
        );
        let context = MsSqlSourceFactory.activation_credential_context(&request)?;
        assert_eq!(
            (context.hostname.as_str(), context.port),
            ("sql.corp", 14330)
        );
        Ok(())
    }

    #[test]
    fn the_sdk_assessment_matrix_agrees_with_the_classification() -> Result<()> {
        let suite = drasi_computation_plugin_sdk::testing::SourceConformance::new(
            &MsSqlSourceFactory,
            "585da1dd-2d7d-5cec-bcf7-e84ac13a88f8",
        )?;
        let base = serde_json::json!({"stream": "items/out", "database": "app", "user": "drasi", "password": "p"});
        let cases: Vec<_> = management::tests_cases()
            .into_iter()
            .map(|(field, value, boundary)| {
                let mut proposed = base.clone();
                proposed[field] = value;
                (proposed, boundary)
            })
            .collect();
        suite.assess_matrix(&base, &cases)
    }
}
