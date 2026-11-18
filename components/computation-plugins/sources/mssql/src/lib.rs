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
pub mod coordinator;
mod management;
mod progress;
mod worker;

use anyhow::{Context, Result};
use config::SourceConfig;
use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateMode, GraphChangeCodec, ImplementationIdentity,
};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, ActivationCredentialContext, ActivationCredentialEndpoint, Capabilities,
    Component, ConfigDetails, ControlSender, CreateRequest, CreatedComponent, Factory,
    FactoryMetadata, HostServices, NativeConsumerProgress, PluginDefinition, ServiceBindings,
    SourceBootstrapCoordination, UpdatePlanner, UpdateRequest,
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

/// The declared description. Outputs carry only source replay progress,
/// kept in the consumers' cursors (`consumer_progress`). The source replays
/// from those cursors and holds a fresh origin until every member committed
/// it, so its progress is exactly as durable as the bound consumer progress
/// declares: durable consumers give a source that resumes after a process
/// restart, volatile consumers one that starts again at `startPosition`.
/// Either binding is accepted.
pub fn description(
    consumer_progress: Option<drasi_computation_contracts::ResourceDurability>,
    descriptor: &drasi_computation_contracts::ComponentDescriptor,
) -> drasi_computation_contracts::ComponentDescription {
    use drasi_computation_contracts::{
        ComponentDescription, DependencyRequirement, DurabilityRequirement, OutputDescription,
        ProgressPersistence, ResourceDurability,
    };
    let progress = match consumer_progress {
        Some(ResourceDurability::Durable) => ProgressPersistence::Durable,
        Some(ResourceDurability::Volatile) => ProgressPersistence::Volatile,
        Some(ResourceDurability::Undeclared) | None => ProgressPersistence::Undeclared,
    };
    ComponentDescription::new(progress)
        .with_outputs(descriptor, OutputDescription::UNTRACKED)
        .with_dependency(
            "consumer_progress",
            DependencyRequirement {
                durability: DurabilityRequirement::Any,
            },
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
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        coordinator::declare(
            request,
            description(
                request.bound_durability("consumer_progress"),
                &request.descriptor,
            ),
        )
    }
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
                // Outputs carry source replay progress, not a producer identity of their
                // own (Host admission, when bound, publishes under its durable identity).
                services: HostServices {
                    consumer_progress: true,
                    bootstrap_coordination: true,
                    activation_credentials: true,
                    ..HostServices::default()
                },
                ..Capabilities::default()
            },
        }
    }
    fn configuration_details(&self) -> ConfigDetails {
        config::definition().details()
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        services: ServiceBindings,
    ) -> Result<CreatedComponent> {
        let progress = services
            .consumer_progress
            .context("SQL Server source requires its actual consumer-progress binding")?;
        Ok(Component::Source(Box::new(self.create_source(
            request,
            &progress,
            tiberius_connectors(),
        )?))
        .into())
    }
    fn update_planner(&self) -> Option<&dyn UpdatePlanner> {
        Some(self)
    }
    fn activation_credentials(&self) -> Option<&dyn ActivationCredentialEndpoint> {
        Some(self)
    }
    fn bootstrap_coordination(&self) -> Option<&dyn SourceBootstrapCoordination> {
        Some(self)
    }
}

impl UpdatePlanner for MsSqlSourceFactory {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        management::assess(request)
    }
}

impl ActivationCredentialEndpoint for MsSqlSourceFactory {
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
}

impl SourceBootstrapCoordination for MsSqlSourceFactory {
    fn create_bootstrap_coordinator(
        &self,
        request: &CreateRequest,
        progress: drasi_computation_plugin_sdk::SourceBootstrapProgress,
    ) -> Result<Arc<dyn drasi_computation_plugin_sdk::NativeSourceBootstrapCoordinator>> {
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        Ok(Arc::new(coordinator::Coordinator::new(
            request,
            SourceConfig::from_value(&request.configuration)?,
            progress,
        )?))
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
        assert!(metadata.capabilities.services.consumer_progress);
        assert!(metadata.capabilities.services.activation_credentials);
        assert!(MsSqlSourceFactory.activation_credentials().is_some());
        let details = MsSqlSourceFactory.configuration_details();
        details.validate_schema(&metadata.configuration)?;
        Ok(())
    }

    /// The source's progress is its consumers' cursors, so it declares
    /// exactly the durability the bound consumer progress declares and
    /// accepts either binding; outputs carry no producer identity.
    #[test]
    fn description_follows_the_bound_consumer_progress_durability() -> Result<()> {
        use drasi_computation_contracts::{
            check_dependency, ComponentDescriptor, OutputDescription, PortId, ProgressPersistence,
            ResourceCapabilities, ResourceDurability,
        };
        let id = drasi_computation_contracts::ComponentId::generate();
        let request = drasi_computation_plugin_sdk::testing::create_request(
            &MsSqlSourceFactory,
            &id,
            serde_json::json!({"stream": "s/out", "database": "d", "user": "u", "password": "p"}),
        );
        let descriptor = ComponentDescriptor::try_new(id, MsSqlSource::ports()?)?;
        for (bound, expected) in [
            (
                Some(ResourceDurability::Durable),
                ProgressPersistence::Durable,
            ),
            (
                Some(ResourceDurability::Volatile),
                ProgressPersistence::Volatile,
            ),
            (
                Some(ResourceDurability::Undeclared),
                ProgressPersistence::Undeclared,
            ),
            (None, ProgressPersistence::Undeclared),
        ] {
            let declared = MsSqlSourceFactory.describe(
                &drasi_computation_plugin_sdk::wire::DescribeRequest {
                    request: request.clone(),
                    descriptor: descriptor.clone(),
                    dependencies: bound
                        .map(|durability| {
                            (
                                "consumer_progress".to_owned(),
                                vec![ResourceCapabilities { durability }],
                            )
                        })
                        .into_iter()
                        .collect(),
                },
            )?;
            assert_eq!(declared.component.progress, expected, "{bound:?}");
            assert_eq!(
                declared.output(&PortId::try_new("out")?),
                OutputDescription::UNTRACKED
            );
            declared
                .validate(&descriptor)
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            for capabilities in [
                ResourceCapabilities::DURABLE,
                ResourceCapabilities::VOLATILE,
            ] {
                assert!(check_dependency(&declared, "consumer_progress", capabilities).is_ok());
            }
        }
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
