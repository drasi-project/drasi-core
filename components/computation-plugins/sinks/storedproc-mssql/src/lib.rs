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
    /// A consumer whose inputs deliver with its configured `deliveryGuarantee`.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        let guarantee = match request.request.configuration.get("deliveryGuarantee") {
            None | Some(serde_json::Value::Null) => {
                drasi_computation_plugin_sdk::DeliveryGuarantee::ExactlyOnce
            }
            Some(value) => serde_json::from_value(value.clone())
                .map_err(|error| anyhow::anyhow!("invalid deliveryGuarantee: {error}"))?,
        };
        Ok(request.consumer(guarantee))
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
                ..Capabilities::default()
            },
        }
    }

    fn configuration_details(&self) -> Option<ConfigDetails> {
        Some(MsSqlStoredProcConfig::definition().details())
    }

    fn consumer_mode(&self) -> Option<ConsumerMode> {
        Some(ConsumerMode::External)
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
        let config = MsSqlStoredProcConfig::parse(request.configuration.clone())?;
        let context = ActivationCredentialContext {
            hostname: config.host,
            port: config.port,
        };
        context.validate()?;
        Ok(context)
    }

    fn create(&self, request: &CreateRequest, _control: ControlSender) -> Result<CreatedComponent> {
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
}

#[cfg(test)]
mod tests;
