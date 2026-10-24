// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use drasi_computation_contracts::{ComponentId, ImplementationIdentity, StreamId};
use drasi_computation_plugin_sdk::{
    BootstrapFactory, BootstrapFactoryMetadata, ConfigSchema, ConfigType, CreateRequest,
    LinkedBootstrapProvider, NativeBootstrapProvider, NativeSourceBootstrap, NativeSourceProgress,
    SourceBootstrapMetadata,
};
use serde::Deserialize;
use std::sync::Arc;

pub const COORDINATED_BOOTSTRAP: &str = "drasi.sqlite/coordinated-bootstrap";
pub const SHARED_BOOTSTRAP: &str = "drasi.sqlite/shared-bootstrap";
pub struct CoordinatedBootstrapFactory;
pub struct SharedBootstrapFactory;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    source: String,
    stream: String,
}

fn metadata(shared: bool) -> BootstrapFactoryMetadata {
    BootstrapFactoryMetadata {
        implementation: ImplementationIdentity::try_new(
            if shared {
                SHARED_BOOTSTRAP
            } else {
                COORDINATED_BOOTSTRAP
            },
            "1",
        )
        .expect("constant implementation"),
        configuration_version: 1,
        configuration: ConfigSchema::from_fields([
            ("source", ConfigType::String, true),
            ("stream", ConfigType::String, true),
        ]),
        source_progress: true,
    }
}

fn validate(
    request: &CreateRequest,
    pairing: &SourceBootstrapMetadata,
    shared: bool,
) -> Result<()> {
    metadata(shared)
        .configuration
        .validate(&request.configuration)?;
    let configuration: Configuration = serde_json::from_value(request.configuration.clone())?;
    let source = ComponentId::try_new(configuration.source)?;
    let stream = StreamId::try_new(configuration.stream)?;
    let scope = request
        .scope
        .as_ref()
        .context("SQLite bootstrap requires query scope")?;
    let bound_scope = pairing
        .source
        .scope
        .as_ref()
        .context("SQLite source binding has no graph scope")?;
    let expected = if shared {
        "drasi.sqlite/shared-transactions"
    } else {
        "drasi.sqlite/replay-transactions"
    };
    anyhow::ensure!(
        source == pairing.source.id
            && pairing.query == request.id
            && scope.instance_id == bound_scope.instance_id
            && scope.graph_id == bound_scope.graph_id,
        "SQLite bootstrap does not match its actual source/query scope"
    );
    anyhow::ensure!(
        pairing.source.implementation.name.as_ref() == expected
            && pairing.source.implementation.version.as_ref() == "1"
            && pairing.source.configuration_version == 1
            && pairing
                .source
                .implementation
                .plugin
                .as_ref()
                .is_some_and(|plugin| plugin.id.as_ref() == "drasi-computation-source-sqlite")
            && pairing.source.configuration.get("coordinated_snapshot")
                == Some(&serde_json::Value::Bool(true))
            && pairing.source.configuration["stream"].as_str() == Some(stream.as_str())
            && pairing.source.configuration["settings"]["output"] == "transactions"
            && pairing.source.configuration["settings"]["replay"].is_object(),
        "SQLite bootstrap source mode or immutable configuration does not match"
    );
    Ok(())
}

impl BootstrapFactory for CoordinatedBootstrapFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        metadata(false)
    }
    fn requires_source_bootstrap(&self) -> bool {
        true
    }
    fn create(
        &self,
        _: &CreateRequest,
        _: Option<NativeSourceProgress>,
    ) -> Result<Arc<dyn NativeBootstrapProvider>> {
        anyhow::bail!(
            "SQLite coordinated bootstrap requires its actual source/bootstrap coordinator"
        )
    }
    fn create_with_source_bootstrap(
        &self,
        request: &CreateRequest,
        source: NativeSourceBootstrap,
    ) -> Result<LinkedBootstrapProvider> {
        validate(request, source.metadata(), false)?;
        source.open()
    }
}

impl BootstrapFactory for SharedBootstrapFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        metadata(true)
    }
    fn requires_source_bootstrap(&self) -> bool {
        true
    }
    fn create(
        &self,
        _: &CreateRequest,
        _: Option<NativeSourceProgress>,
    ) -> Result<Arc<dyn NativeBootstrapProvider>> {
        anyhow::bail!("SQLite shared bootstrap requires its actual source/bootstrap coordinator")
    }
    fn create_with_source_bootstrap(
        &self,
        request: &CreateRequest,
        source: NativeSourceBootstrap,
    ) -> Result<LinkedBootstrapProvider> {
        validate(request, source.metadata(), true)?;
        source.open()
    }
}
