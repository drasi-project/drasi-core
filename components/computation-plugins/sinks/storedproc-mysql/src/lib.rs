// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! `drasi.mysql/storedproc`: native MySQL stored-procedure delivery.
//!
//! An external consumer of the host `DeliveryRunner`. Each operation of an
//! input runs one rendered `CALL` with positional `?` parameters inside its
//! own MySQL transaction, which also advances this component's row in a
//! destination cursor table. The effect and its receipt commit together, so
//! a retried or replayed operation is not applied twice unless the procedure
//! ends the transaction itself. See `DESIGN.md` and `CONFIGURATION.md`.

mod component;
pub mod config;
pub mod management;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateMode, DeliveryScope, GraphChangeCodec, ImplementationIdentity,
    SinkCompletion,
};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, ActivationCredentialContext, Capabilities, Component, ConfigDetails,
    ConsumerMode, ControlSender, CreateRequest, CreatedComponent, Factory, FactoryMetadata,
    PluginDefinition, UpdateRequest,
};
use std::sync::Arc;

pub use component::MySqlStoredProcSink;
pub use config::MySqlStoredProcConfig;
pub use drasi_computation_storedproc_common::owner;
pub use drasi_reaction_common::{QueryConfig, TemplateSpec};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const MYSQL_STOREDPROC: &str = "drasi.mysql/storedproc";

pub struct MySqlStoredProcFactory;

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(MySqlStoredProcFactory)],
        vec![GraphChangeCodec::schema()],
    )
}
#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for MySqlStoredProcFactory {
    /// A consumer whose inputs deliver with its configured
    /// `deliveryGuarantee` and which applies its `onPermanentFailure`.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        let config = MySqlStoredProcConfig::parse_with_activation_credentials(
            request.request.configuration.clone(),
            request.request.activation_credentials_bound,
        )?;
        Ok(request.consumer(config.delivery_guarantee, config.on_permanent_failure))
    }
    fn supports_activation_credentials(&self) -> bool {
        true
    }
    fn activation_credential_context(
        &self,
        request: &CreateRequest,
    ) -> Result<ActivationCredentialContext> {
        let config = MySqlStoredProcConfig::parse_with_activation_credentials(
            request.configuration.clone(),
            true,
        )?;
        let context = ActivationCredentialContext {
            hostname: config.host,
            port: config.port,
        };
        context.validate()?;
        Ok(context)
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
    fn consumer_mode(&self) -> Option<ConsumerMode> {
        Some(ConsumerMode::External)
    }
    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(MYSQL_STOREDPROC, "1")
                .expect("constant implementation identity"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: config::definition().schema(),
            ports: MySqlStoredProcSink::ports().expect("constant port descriptor"),
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
    fn create(&self, request: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        let config = MySqlStoredProcConfig::parse_with_activation_credentials(
            request.configuration.clone(),
            request.activation_credentials_bound,
        )?;
        let scope = request
            .scope
            .as_ref()
            .context("the MySQL stored-procedure sink requires its graph scope")?;
        let scope = DeliveryScope::new(
            scope.instance_id.clone(),
            scope.graph_id.clone(),
            request.id.clone(),
        )?;
        Ok(Component::Consumer(Box::new(MySqlStoredProcSink::new(
            request.id.clone(),
            config,
            request.activation_credentials_bound,
            &request.inputs,
            scope,
        )?))
        .into())
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod lifecycle_ctx {
    use drasi_computation_plugin_sdk::lifecycle::{
        CancellationToken, DisposeContext, DisposeReason, InitializeContext, OperationScope,
        StartContext, StateIntent, StopContext, StopMode,
    };

    pub(crate) fn scope() -> OperationScope {
        OperationScope::new("test", "component", 1, None, CancellationToken::new())
    }
    pub(crate) fn initialize() -> InitializeContext {
        InitializeContext::new(scope())
    }
    pub(crate) fn start() -> StartContext {
        StartContext::new(scope(), None)
    }
    pub(crate) fn start_with(
        credentials: drasi_computation_plugin_sdk::ActivationCredentials,
    ) -> StartContext {
        StartContext::new(scope(), Some(credentials))
    }
    pub(crate) fn stop() -> StopContext {
        StopContext::new(scope(), StopMode::Drain)
    }
    pub(crate) fn erase() -> DisposeContext {
        DisposeContext::new(scope(), DisposeReason::Removal, StateIntent::Erase, 1)
            .expect("valid dispose context")
    }
}
