// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native SQL Server stored-procedure sink (`drasi.mssql/storedproc`).
//!
//! An external consumer of the host `DeliveryRunner`: each operation runs in
//! its own SQL Server transaction that executes the rendered procedure call
//! and advances this sink's destination cursor row, so an operation commits
//! exactly once when the procedure does not commit on its own. Templates are
//! parameter-only: every value is bound as `@P1…@Pn`.
//!
//! See `DESIGN.md` and `CONFIGURATION.md` in this crate. Live SQL Server
//! qualification is deferred (CW-78).

mod component;
pub mod config;
pub mod delivery;
pub mod management;
mod session;

use anyhow::Result;
use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateMode, GraphChangeCodec, ImplementationIdentity, SinkCompletion,
};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, ActivationCredentialContext, Capabilities, Component, ConfigDetails,
    ConsumerMode, ControlSender, CreateRequest, CreatedComponent, Factory, FactoryMetadata,
    PluginDefinition, UpdateRequest,
};
use std::sync::Arc;

pub use component::MsSqlStoredProcSink;
pub use config::{Encryption, MsSqlStoredProcConfig, ProgressTable};
pub use drasi_computation_storedproc_common::owner;
pub use drasi_reaction_common::{OperationType, QueryConfig, TemplateSpec};
pub use session::{create_table_sql, Authentication};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const MSSQL_SINK: &str = "drasi.mssql/storedproc";

/// Factory for `drasi.mssql/storedproc`.
pub struct MsSqlStoredProcFactory;

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(MsSqlStoredProcFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for MsSqlStoredProcFactory {
    /// A consumer whose inputs deliver with its configured
    /// `deliveryGuarantee` and which applies its `onPermanentFailure`.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        let config = MsSqlStoredProcConfig::parse(request.request.configuration.clone())?;
        Ok(request.consumer(config.delivery_guarantee, config.on_permanent_failure))
    }
    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            // Constant identifiers; validated by `ExportedPlugin::new`.
            implementation: ImplementationIdentity::try_new(MSSQL_SINK, "1")
                .expect("constant implementation identity"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: MsSqlStoredProcConfig::definition().schema(),
            ports: MsSqlStoredProcSink::ports().expect("constant port descriptor"),
            completion: Some(SinkCompletion::Handled),
            capabilities: Capabilities {
                binds_inputs: true,
                management: true,
                update_modes: vec![
                    ComponentUpdateMode::InPlace,
                    ComponentUpdateMode::Restart,
                    ComponentUpdateMode::Recreate,
                ],
                erases_owned_state: true,
                services: drasi_computation_plugin_sdk::HostServices {
                    consumer: Some(ConsumerMode::External),
                    activation_credentials: true,
                    ..Default::default()
                },
                ..Capabilities::default()
            },
        }
    }

    fn configuration_details(&self) -> ConfigDetails {
        MsSqlStoredProcConfig::definition().details()
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        _services: drasi_computation_plugin_sdk::ServiceBindings,
    ) -> Result<CreatedComponent> {
        let config = MsSqlStoredProcConfig::parse(request.configuration.clone())?;
        let scope = request.scope.as_ref().ok_or_else(|| {
            anyhow::anyhow!("the SQL Server sink requires its graph scope (cursor owner)")
        })?;
        Ok(Component::Consumer(Box::new(MsSqlStoredProcSink::new(
            request.id.clone(),
            &scope.graph_id,
            config,
            &request.inputs,
            request.activation_credentials_bound,
        )?))
        .into())
    }
    fn update_planner(&self) -> Option<&dyn drasi_computation_plugin_sdk::UpdatePlanner> {
        Some(self)
    }
    fn activation_credentials(
        &self,
    ) -> Option<&dyn drasi_computation_plugin_sdk::ActivationCredentialEndpoint> {
        Some(self)
    }
}

impl drasi_computation_plugin_sdk::UpdatePlanner for MsSqlStoredProcFactory {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        management::assess(request)
    }
}

impl drasi_computation_plugin_sdk::ActivationCredentialEndpoint for MsSqlStoredProcFactory {
    fn activation_credential_context(
        &self,
        request: &CreateRequest,
    ) -> Result<ActivationCredentialContext> {
        let config = MsSqlStoredProcConfig::parse(request.configuration.clone())?;
        let context = ActivationCredentialContext {
            hostname: config.host,
            port: config.port,
        };
        context.validate()?;
        Ok(context)
    }
}

#[cfg(test)]
mod tests;
