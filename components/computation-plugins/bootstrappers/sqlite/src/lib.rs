// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod coordinated;
mod snapshot;

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

pub use coordinated::{
    CoordinatedBootstrapFactory, SharedBootstrapFactory, COORDINATED_BOOTSTRAP, SHARED_BOOTSTRAP,
};
pub use drasi_computation_plugin_sdk::NativeBootstrapProvider;
pub use drasi_sqlite_common::TableKeyConfig;
pub use snapshot::{SqliteBootstrapConfig, SqliteBootstrapProvider};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const FILE_BOOTSTRAP: &str = "drasi.sqlite/file-bootstrap";

pub struct FileBootstrapFactory;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    source: String,
    stream: String,
    settings: SqliteBootstrapConfig,
}

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new_with_bootstrap(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        Vec::new(),
        vec![
            Arc::new(FileBootstrapFactory),
            Arc::new(CoordinatedBootstrapFactory),
            Arc::new(SharedBootstrapFactory),
        ],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl BootstrapFactory for FileBootstrapFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        BootstrapFactoryMetadata {
            implementation: ImplementationIdentity::try_new(FILE_BOOTSTRAP, "1")
                .expect("constant implementation"),
            configuration_version: 1,
            configuration: ConfigSchema::from_fields([
                ("source", ConfigType::String, true),
                ("stream", ConfigType::String, true),
                ("settings", ConfigType::Object, true),
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
            "file bootstrap does not bind persistent source progress"
        );
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config: Configuration = serde_json::from_value(request.configuration.clone())?;
        let scope = request
            .scope
            .clone()
            .context("SQLite file bootstrap requires query scope")?;
        ComponentId::try_new(scope.instance_id.as_str())?;
        ComponentId::try_new(scope.graph_id.as_str())?;
        let mut provider = SqliteBootstrapProvider::new(
            ComponentId::try_new(config.source)?,
            StreamId::try_new(config.stream)?,
            config.settings,
        )?;
        provider.query_scope = Some((scope, request.id.clone()));
        Ok(Arc::new(provider))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_plugin_sdk::{ExportedPlugin, Scope};
    use serde_json::json;

    fn request() -> Result<CreateRequest> {
        Ok(CreateRequest {
            activation_credentials_bound: false,
            id: ComponentId::try_new("query")?,
            implementation: FileBootstrapFactory.metadata().implementation,
            configuration_version: 1,
            scope: Some(Scope {
                instance_id: "instance".into(),
                graph_id: "graph".into(),
                generation: 1,
            }),
            configuration: json!({
                "source": "source",
                "stream": "source/out",
                "settings": {
                    "path": "a-file-that-must-not-be-opened-at-construction.db",
                    "tables": [],
                    "max_tables": 16,
                    "max_bytes": 65536,
                    "max_duration_ms": 1000,
                    "output_capacity": 1,
                    "shutdown_timeout_ms": 1000
                }
            }),
        })
    }

    #[test]
    fn native_factory_is_configuration_only_and_query_scoped() -> Result<()> {
        ExportedPlugin::new(plugin()?)?;
        let request = request()?;
        let provider = FileBootstrapFactory.create(&request, None)?;
        provider.validate_query_scope("instance", "graph", &request.id)?;
        assert!(provider
            .validate_query_scope("another", "graph", &request.id)
            .is_err());
        assert!(provider
            .validate_query_scope("instance", "another", &request.id)
            .is_err());
        assert!(provider
            .validate_query_scope("instance", "graph", &ComponentId::try_new("other")?)
            .is_err());
        assert!(!FileBootstrapFactory.metadata().source_progress);
        Ok(())
    }

    #[test]
    fn native_factory_rejects_unknown_options_invalid_bounds_and_absent_scope() -> Result<()> {
        for field in ["unknown", "scope", "source", "stream", "bounds", "settings"] {
            let mut request = request()?;
            match field {
                "unknown" => request.configuration["extra"] = json!(true),
                "scope" => request.scope = None,
                "source" => request.configuration["source"] = json!(""),
                "stream" => request.configuration["stream"] = json!(""),
                "bounds" => request.configuration["settings"]["output_capacity"] = json!(0),
                "settings" => request.configuration["settings"]["replay"] = json!(true),
                _ => unreachable!(),
            }
            assert!(
                FileBootstrapFactory.create(&request, None).is_err(),
                "{field}"
            );
        }
        Ok(())
    }
}
