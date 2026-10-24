// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod snapshot;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use drasi_computation_contracts::{ComponentId, GraphChangeCodec, ImplementationIdentity};
use drasi_computation_plugin_sdk::{
    BootstrapFactory, BootstrapFactoryMetadata, ConfigSchema, ConfigType, CreateRequest,
    NativeBootstrapProvider, NativeSourceProgress, PluginDefinition,
};
use std::sync::Arc;

pub use drasi_http_common::bootstrap::config as settings;
pub use snapshot::{HttpBootstrapConfig, HttpBootstrapProvider};

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
            configuration: ConfigSchema::from_fields([
                ("source", ConfigType::String, true),
                ("stream", ConfigType::String, true),
                ("settings", ConfigType::Object, true),
                ("nodeLabels", ConfigType::Array, false),
                ("relationLabels", ConfigType::Array, false),
                ("maxResponseBytes", ConfigType::Integer, false),
                ("outputCapacity", ConfigType::Integer, false),
                ("maxDurationMs", ConfigType::Integer, false),
                ("shutdownTimeoutMs", ConfigType::Integer, false),
            ]),
            source_progress: false,
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        progress: Option<NativeSourceProgress>,
    ) -> Result<Arc<dyn NativeBootstrapProvider>> {
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
        ComponentId::try_new(scope.instance_id.as_str())?;
        ComponentId::try_new(scope.graph_id.as_str())?;
        let mut provider =
            HttpBootstrapProvider::new(serde_json::from_value(request.configuration.clone())?)?;
        provider.query_scope = Some((scope, request.id.clone()));
        Ok(Arc::new(provider))
    }
}
