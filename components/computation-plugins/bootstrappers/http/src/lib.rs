// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

pub(crate) mod snapshot;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use drasi_computation_contracts::{ComponentId, GraphChangeCodec, ImplementationIdentity};
use drasi_computation_plugin_sdk::{
    BootstrapCompletionKind, BootstrapFactory, BootstrapFactoryMetadata,
    BootstrapOutputDescription, BootstrapSession, CreateRequest, ElementNamespace,
    NativeBootstrapProvider, NativeSourceProgress, PluginDefinition,
};
use std::sync::Arc;

pub use drasi_http_common::bootstrap::config as settings;
pub use snapshot::{definition, HttpBootstrapConfig, HttpBootstrapProvider};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const HTTP_BOOTSTRAP: &str = "drasi.network/http-bootstrap";

pub struct HttpBootstrapFactory;

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new_with_bootstrap(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        Vec::new(),
        vec![Arc::new(HttpBootstrapFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl BootstrapFactory for HttpBootstrapFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        BootstrapFactoryMetadata {
            implementation: ImplementationIdentity::try_new(HTTP_BOOTSTRAP, "1")
                .expect("constant implementation"),
            configuration_version: 1,
            configuration: snapshot::definition().schema(),
            source_progress: false,
            independent_state: false,
            pairing: None,
        }
    }

    /// A quiescent, unpaired provider in the namespace of the source it is
    /// connected to.
    fn describe(&self, configuration: &serde_json::Value) -> Result<BootstrapOutputDescription> {
        self.metadata().configuration.validate(configuration)?;
        HttpBootstrapConfig::from_value(configuration)?;
        Ok(BootstrapOutputDescription {
            completion: BootstrapCompletionKind::Quiescent,
            pairing: None,
            namespace: ElementNamespace::ServedSource,
            identity: None,
        })
    }

    fn create(
        &self,
        request: &CreateRequest,
        session: &BootstrapSession,
        progress: Option<NativeSourceProgress>,
    ) -> Result<NativeBootstrapProvider> {
        anyhow::ensure!(
            progress.is_none(),
            "HTTP bootstrap does not establish a persistent source boundary"
        );
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let scope = request
            .scope
            .clone()
            .context("HTTP bootstrap requires query scope")?;
        drasi_computation_contracts::validate_identifier("instance", &scope.instance_id)?;
        drasi_computation_contracts::validate_identifier("graph", &scope.graph_id)?;
        let mut provider = HttpBootstrapProvider::new(
            serde_json::from_value(request.configuration.clone())?,
            session.served()?.clone(),
            session.labels.clone(),
        )?;
        provider.query_scope = Some((scope, request.id.clone()));
        Ok(NativeBootstrapProvider::Quiescent(Arc::new(provider)))
    }
}
