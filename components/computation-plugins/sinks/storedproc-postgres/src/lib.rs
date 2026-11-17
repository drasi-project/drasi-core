// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native PostgreSQL stored-procedure sink (`drasi.postgres/storedproc`).
//!
//! An external consumer for the host `DeliveryRunner`: every operation's
//! procedure call commits in one PostgreSQL transaction together with a
//! per-consumer destination cursor row, so a retried operation whose earlier
//! attempt committed is recognised and not repeated. See `DESIGN.md` and
//! `CONFIGURATION.md` in this crate.

mod component;
pub mod config;
mod connection;
pub mod management;
mod retry;

use anyhow::{Context, Result};
use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateData, ComponentUpdateMode, GraphChangeCodec,
    ImplementationIdentity, PipeRequirements, PortDescriptor, PortDirection, PortId,
    SinkCompletion, SinkOutputs,
};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, ActivationCredentialContext, Capabilities, Component, ConfigDetails,
    ConsumerMode, ControlSender, CreateRequest, CreatedComponent, Factory, FactoryMetadata,
    PluginDefinition, UpdateRequest,
};
use std::sync::Arc;

pub use component::{PostgresStoredProcSink, RenderError};
pub use config::{CommandRoute, CommandSpec, PostgresStoredProcConfig, SslMode};
pub use connection::{ConnectError, APPLICATION_NAME};
pub use drasi_postgres_common::delivery::progress::{
    create_table_sql, DeliveryError, DeliveryOwner, Disposition, ProgressTable, ProgressTableError,
    DEFAULT_MAX_STREAMS, DEFAULT_PROGRESS_TABLE, PROGRESS_FORMAT_VERSION,
};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const POSTGRES_SINK: &str = "drasi.postgres/storedproc";

/// Factory for `drasi.postgres/storedproc`.
pub struct PostgresStoredProcFactory;

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(PostgresStoredProcFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

fn input_port() -> PortDescriptor {
    PortDescriptor::new(
        PortId::try_new("in").expect("constant port"),
        PortDirection::Input,
        GraphChangeCodec::schema().descriptor().clone(),
        PipeRequirements::default(),
    )
}

impl PostgresStoredProcFactory {
    /// Constructs the consumer for a creation request without I/O.
    pub fn build(&self, request: &CreateRequest) -> Result<PostgresStoredProcSink> {
        if let Some(directive) = &request.update_directive {
            anyhow::ensure!(
                directive.plan.mode == ComponentUpdateMode::Recreate
                    && directive.plan.data == ComponentUpdateData::Preserve,
                "PostgreSQL sink reconstruction preserves its destination cursor; only Recreate \
                 with Preserve is supported"
            );
        }
        let config = PostgresStoredProcConfig::parse(request.configuration.clone())?;
        let scope = request.scope.as_ref().context(
            "PostgreSQL sink requires its construction scope: the graph identity names the \
             destination cursor owner",
        )?;
        let owner = DeliveryOwner::derive(&scope.graph_id, request.id.as_str());
        let outputs = SinkOutputs::from_inputs(&request.inputs, &config.outputs)?;
        PostgresStoredProcSink::new(
            self.metadata().descriptor(request.id.clone())?,
            config,
            outputs,
            owner,
        )
    }
}

impl Factory for PostgresStoredProcFactory {
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
            // Constant identifiers; validated by the factory tests.
            implementation: ImplementationIdentity::try_new(POSTGRES_SINK, "1")
                .expect("constant implementation identity"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: PostgresStoredProcConfig::definition().schema(),
            ports: vec![input_port()],
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
        Some(PostgresStoredProcConfig::definition().details())
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

    /// The configured endpoint; pure configuration inspection.
    fn activation_credential_context(
        &self,
        request: &CreateRequest,
    ) -> Result<ActivationCredentialContext> {
        let config = PostgresStoredProcConfig::parse(request.configuration.clone())?;
        let context = ActivationCredentialContext {
            hostname: config.host,
            port: config.port,
        };
        context.validate()?;
        Ok(context)
    }

    fn consumer_mode(&self) -> Option<ConsumerMode> {
        Some(ConsumerMode::External)
    }

    fn create(&self, request: &CreateRequest, _control: ControlSender) -> Result<CreatedComponent> {
        Ok(Component::Consumer(Box::new(self.build(request)?)).into())
    }
}

#[cfg(test)]
mod tests;
