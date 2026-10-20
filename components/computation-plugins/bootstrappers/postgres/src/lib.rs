// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod shared;
mod snapshot;
mod standalone;

use anyhow::{Context, Result};
use drasi_computation_contracts::{
    ComponentId, GraphChangeCodec, ImplementationIdentity, StreamId,
};
use drasi_computation_plugin_sdk::{
    BootstrapFactory, BootstrapFactoryMetadata, ConfigSchema, ConfigType, CreateRequest,
    NativeSourceProgress, PluginDefinition,
};
use serde::Deserialize;
use std::sync::Arc;

pub use drasi_computation_plugin_sdk::NativeBootstrapProvider;
pub use drasi_postgres_common::replication::PostgresTransactionConfig;
pub use shared::PostgresSharedSnapshot;
pub use snapshot::PostgresSnapshot;
pub use standalone::{PostgresBootstrapConfig, PostgresBootstrapProvider};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const POSTGRES_BOOTSTRAP: &str = "drasi.postgres/coordinated-bootstrap";
pub const POSTGRES_SNAPSHOT: &str = "drasi.postgres/snapshot";
pub const POSTGRES_ROW_BOOTSTRAP: &str = "drasi.postgres/coordinated-row-bootstrap";
pub const POSTGRES_SHARED_ROW_BOOTSTRAP: &str = "drasi.postgres/shared-row-bootstrap";
pub const POSTGRES_SHARED_TRANSACTION_BOOTSTRAP: &str =
    "drasi.postgres/shared-transaction-bootstrap";

pub struct PostgresBootstrapFactory;
pub struct StandalonePostgresBootstrapFactory;
pub struct PostgresRowBootstrapFactory;
pub struct PostgresSharedRowBootstrapFactory;
pub struct PostgresSharedTransactionBootstrapFactory;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    source: String,
    stream: String,
    settings: PostgresTransactionConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StandaloneConfiguration {
    source: String,
    stream: String,
    settings: PostgresBootstrapConfig,
}

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new_with_bootstrap(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        Vec::new(),
        vec![
            Arc::new(PostgresBootstrapFactory),
            Arc::new(StandalonePostgresBootstrapFactory),
            Arc::new(PostgresRowBootstrapFactory),
            Arc::new(PostgresSharedRowBootstrapFactory),
            Arc::new(PostgresSharedTransactionBootstrapFactory),
        ],
        vec![GraphChangeCodec::schema()],
    )
}

impl BootstrapFactory for PostgresSharedTransactionBootstrapFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        let mut metadata = PostgresBootstrapFactory.metadata();
        metadata.implementation =
            ImplementationIdentity::try_new(POSTGRES_SHARED_TRANSACTION_BOOTSTRAP, "1")
                .expect("constant implementation");
        metadata
    }
    fn create(
        &self,
        request: &CreateRequest,
        progress: Option<NativeSourceProgress>,
    ) -> Result<Arc<dyn NativeBootstrapProvider>> {
        let (config, scope, reader) = scoped_configuration(request, progress)?;
        let mut provider = PostgresSharedSnapshot::transactions(
            ComponentId::try_new(config.source)?,
            StreamId::try_new(config.stream)?,
            config.settings,
            reader,
        )?;
        provider.query_scope = Some((scope, request.id.clone()));
        Ok(Arc::new(provider))
    }
}

impl BootstrapFactory for PostgresSharedRowBootstrapFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        let mut metadata = PostgresBootstrapFactory.metadata();
        metadata.implementation =
            ImplementationIdentity::try_new(POSTGRES_SHARED_ROW_BOOTSTRAP, "1")
                .expect("constant implementation");
        metadata
    }

    fn create(
        &self,
        request: &CreateRequest,
        progress: Option<NativeSourceProgress>,
    ) -> Result<Arc<dyn NativeBootstrapProvider>> {
        let (config, scope, reader) = scoped_configuration(request, progress)?;
        let mut provider = PostgresSharedSnapshot::new(
            ComponentId::try_new(config.source)?,
            StreamId::try_new(config.stream)?,
            config.settings,
            reader,
        )?;
        provider.query_scope = Some((scope, request.id.clone()));
        Ok(Arc::new(provider))
    }
}

impl BootstrapFactory for StandalonePostgresBootstrapFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        let mut metadata = PostgresBootstrapFactory.metadata();
        metadata.implementation = ImplementationIdentity::try_new(POSTGRES_SNAPSHOT, "1")
            .expect("constant implementation");
        metadata.source_progress = false;
        metadata
    }

    fn create(
        &self,
        request: &CreateRequest,
        progress: Option<NativeSourceProgress>,
    ) -> Result<Arc<dyn NativeBootstrapProvider>> {
        anyhow::ensure!(
            progress.is_none(),
            "standalone PostgreSQL snapshots do not own source progress"
        );
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config: StandaloneConfiguration =
            serde_json::from_value(request.configuration.clone())?;
        let scope = request
            .scope
            .clone()
            .context("PostgreSQL snapshot requires query scope")?;
        ComponentId::try_new(scope.instance_id.as_str())?;
        ComponentId::try_new(scope.graph_id.as_str())?;
        let mut provider = PostgresBootstrapProvider::new(
            ComponentId::try_new(config.source)?,
            StreamId::try_new(config.stream)?,
            config.settings,
        )?;
        provider.query_scope = Some((scope, request.id.clone()));
        Ok(Arc::new(provider))
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl BootstrapFactory for PostgresRowBootstrapFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        let mut metadata = PostgresBootstrapFactory.metadata();
        metadata.implementation = ImplementationIdentity::try_new(POSTGRES_ROW_BOOTSTRAP, "1")
            .expect("constant implementation");
        metadata
    }
    fn create(
        &self,
        request: &CreateRequest,
        progress: Option<NativeSourceProgress>,
    ) -> Result<Arc<dyn NativeBootstrapProvider>> {
        create_coordinated(request, progress, true)
    }
}

impl BootstrapFactory for PostgresBootstrapFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        let mut configuration = ConfigSchema::from_fields([
            ("source", ConfigType::String, true),
            ("stream", ConfigType::String, true),
            ("settings", ConfigType::Object, true),
        ]);
        configuration
            .fields
            .get_mut("settings")
            .expect("settings")
            .secret = true;
        BootstrapFactoryMetadata {
            implementation: ImplementationIdentity::try_new(POSTGRES_BOOTSTRAP, "1")
                .expect("constant implementation"),
            configuration_version: 1,
            configuration,
            source_progress: true,
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        progress: Option<NativeSourceProgress>,
    ) -> Result<Arc<dyn NativeBootstrapProvider>> {
        create_coordinated(request, progress, false)
    }
}

fn create_coordinated(
    request: &CreateRequest,
    progress: Option<NativeSourceProgress>,
    rows: bool,
) -> Result<Arc<dyn NativeBootstrapProvider>> {
    let (config, scope, reader) = scoped_configuration(request, progress)?;
    let constructor = if rows {
        PostgresSnapshot::rows
    } else {
        PostgresSnapshot::new
    };
    let mut provider = constructor(
        ComponentId::try_new(config.source)?,
        StreamId::try_new(config.stream)?,
        config.settings,
        reader,
    )?;
    provider.query_scope = Some((scope, request.id.clone()));
    Ok(Arc::new(provider))
}

fn scoped_configuration(
    request: &CreateRequest,
    progress: Option<NativeSourceProgress>,
) -> Result<(
    Configuration,
    drasi_computation_plugin_sdk::Scope,
    drasi_computation_contracts::SourceProgressView,
)> {
    PostgresBootstrapFactory
        .metadata()
        .configuration
        .validate(&request.configuration)?;
    let config: Configuration = serde_json::from_value(request.configuration.clone())?;
    let scope = request
        .scope
        .clone()
        .context("PostgreSQL bootstrap requires query scope")?;
    ComponentId::try_new(scope.instance_id.as_str())?;
    ComponentId::try_new(scope.graph_id.as_str())?;
    let reader = progress
        .context("PostgreSQL bootstrap requires its actual query progress owner")?
        .reader();
    anyhow::ensure!(
        reader.graph_id() == scope.graph_id && reader.component_id() == &request.id,
        "PostgreSQL bootstrap progress belongs to another query"
    );
    Ok((config, scope, reader))
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_plugin_sdk::ExportedPlugin;

    #[test]
    fn bootstrap_library_has_only_a_query_scoped_recoverable_factory() -> Result<()> {
        ExportedPlugin::new(plugin()?)?;
        let metadata = PostgresBootstrapFactory.metadata();
        assert!(metadata.source_progress);
        assert!(metadata.configuration.fields["settings"].secret);
        Ok(())
    }

    #[test]
    fn standalone_factory_needs_no_replication_or_progress_and_is_configuration_only() -> Result<()>
    {
        use drasi_computation_plugin_sdk::Scope;
        let request = CreateRequest {
            id: ComponentId::try_new("query")?,
            implementation: StandalonePostgresBootstrapFactory.metadata().implementation,
            configuration_version: 1,
            scope: Some(Scope {
                instance_id: "instance".into(),
                graph_id: "graph".into(),
                generation: 1,
            }),
            configuration: serde_json::json!({
                "source": "seed", "stream": "seed/out", "settings": {
                    "host": "must-not-connect-at-construction", "database": "db", "user": "reader",
                    "tables": ["items"], "max_tables": 8, "max_protocol_bytes": 65536,
                    "io_timeout_ms": 1000, "max_duration_ms": 10000
                }
            }),
        };
        let provider = StandalonePostgresBootstrapFactory.create(&request, None)?;
        provider.validate_query_scope("instance", "graph", &request.id)?;
        assert!(provider.recovery_reader().is_none());
        assert!(
            !StandalonePostgresBootstrapFactory
                .metadata()
                .source_progress
        );
        assert!(provider
            .validate_query_scope("other", "graph", &request.id)
            .is_err());
        Ok(())
    }
}
