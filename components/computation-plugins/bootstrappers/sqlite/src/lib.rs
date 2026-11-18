// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native SQLite initial-data providers.
//!
//! - `drasi.sqlite/file-bootstrap` reads a database file read-only and emits
//!   every selected row with the source's element identities. It does not
//!   establish a replay boundary: order it before live writes. It serves the
//!   source whose bootstrap input it is connected to (that source's namespace
//!   and stream) and declares its element identities, which the graph
//!   compares with the source's when the edge binds.
//! - `drasi.sqlite/coordinated-bootstrap` and `drasi.sqlite/shared-bootstrap`
//!   pair with a `drasi.sqlite/replay-transactions` or
//!   `drasi.sqlite/shared-transactions` source configured with
//!   `coordinatedSnapshot: true`; the source's coordinator takes the snapshot
//!   with the source's own settings before it admits live writes. They take no
//!   configuration.
//!
//! Every provider reads only the tables whose label a query declares.
//! Configuration is flat camelCase, with the same field names (`path`,
//! `tables`, `tableKeys`) as the source factories.

mod coordinated;
mod snapshot;

use anyhow::{Context, Result};
use drasi_computation_contracts::{GraphChangeCodec, ImplementationIdentity};
use drasi_computation_plugin_sdk::{
    BootstrapCompletionKind, BootstrapFactory, BootstrapFactoryMetadata,
    BootstrapOutputDescription, BootstrapSession, ConfigDefinition, ConfigOption, ConfigType,
    CreateRequest, ElementNamespace, NativeBootstrapProvider, NativeSourceProgress,
    PluginDefinition,
};
use serde::Deserialize;
use serde_json::json;
use std::{
    num::{NonZeroU64, NonZeroUsize},
    path::PathBuf,
    sync::Arc,
};

pub use coordinated::{
    coordinated_factory, shared_factory, COORDINATED_BOOTSTRAP, SHARED_BOOTSTRAP,
};
pub use drasi_computation_plugin_sdk::QuiescentBootstrapProvider;
pub use drasi_sqlite_common::TableKeyConfig;
pub use snapshot::{SqliteBootstrapConfig, SqliteBootstrapProvider};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const FILE_BOOTSTRAP: &str = "drasi.sqlite/file-bootstrap";

pub struct FileBootstrapFactory;

pub const DEFAULT_MAX_TABLES: usize = 1024;
pub const DEFAULT_MAX_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_DURATION_MS: u64 = 300_000;
pub const DEFAULT_OUTPUT_CAPACITY: usize = 16;
pub const DEFAULT_SHUTDOWN_TIMEOUT_MS: u64 = 10_000;

fn default_max_tables() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_MAX_TABLES).expect("nonzero constant")
}
fn default_max_bytes() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_MAX_BYTES).expect("nonzero constant")
}
fn default_max_duration_ms() -> NonZeroU64 {
    NonZeroU64::new(DEFAULT_MAX_DURATION_MS).expect("nonzero constant")
}
fn default_output_capacity() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_OUTPUT_CAPACITY).expect("nonzero constant")
}
fn default_shutdown_timeout_ms() -> NonZeroU64 {
    NonZeroU64::new(DEFAULT_SHUTDOWN_TIMEOUT_MS).expect("nonzero constant")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TableKey {
    table: String,
    key_columns: Vec<String>,
}

/// The flat file-bootstrap configuration.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Configuration {
    path: PathBuf,
    #[serde(default)]
    tables: Vec<String>,
    #[serde(default)]
    table_keys: Vec<TableKey>,
    #[serde(default = "default_max_tables")]
    max_tables: NonZeroUsize,
    #[serde(default = "default_max_bytes")]
    max_bytes: NonZeroUsize,
    #[serde(default = "default_max_duration_ms")]
    max_duration_ms: NonZeroU64,
    #[serde(default = "default_output_capacity")]
    output_capacity: NonZeroUsize,
    #[serde(default = "default_shutdown_timeout_ms")]
    shutdown_timeout_ms: NonZeroU64,
}

impl Configuration {
    fn table_keys(&self) -> Vec<TableKeyConfig> {
        self.table_keys
            .iter()
            .map(|key| TableKeyConfig {
                table: key.table.clone(),
                key_columns: key.key_columns.clone(),
            })
            .collect()
    }

    fn settings(&self) -> SqliteBootstrapConfig {
        SqliteBootstrapConfig {
            path: self.path.clone(),
            tables: self.tables.clone(),
            table_keys: self
                .table_keys
                .iter()
                .map(|key| TableKeyConfig {
                    table: key.table.clone(),
                    key_columns: key.key_columns.clone(),
                })
                .collect(),
            max_tables: self.max_tables,
            max_bytes: self.max_bytes,
            max_duration_ms: self.max_duration_ms,
            output_capacity: self.output_capacity,
            shutdown_timeout_ms: self.shutdown_timeout_ms,
        }
    }
}

/// The published file-bootstrap configuration.
pub fn file_definition() -> ConfigDefinition {
    let integer = |name: &str, description: &str, default: u64| {
        ConfigOption::new(name, ConfigType::Integer, false, description)
            .default(default)
            .minimum(1)
    };
    ConfigDefinition::try_new([
        ConfigOption::new(
            "path",
            ConfigType::String,
            true,
            "Database file, opened read-only. It must exist.",
        ),
        ConfigOption::new(
            "tables",
            ConfigType::Array,
            false,
            "Tables to read; empty reads every ordinary table. Must equal the served source's \
             selection (checked when the provider's edge binds); a query's labels narrow it.",
        )
        .default(json!([])),
        ConfigOption::new(
            "tableKeys",
            ConfigType::Array,
            false,
            "Explicit keys, as {\"table\": \"name\", \"keyColumns\": [..]}. Must equal the served \
             source's keys (checked when the provider's edge binds).",
        )
        .default(json!([])),
        integer(
            "maxTables",
            "Largest number of tables read.",
            DEFAULT_MAX_TABLES as u64,
        ),
        integer(
            "maxBytes",
            "Bound on schema metadata and on each emitted row, in bytes.",
            DEFAULT_MAX_BYTES as u64,
        )
        .maximum(i32::MAX as u64),
        integer(
            "maxDurationMs",
            "Deadline for the whole snapshot, and SQLite's busy timeout.",
            DEFAULT_MAX_DURATION_MS,
        )
        .maximum(i32::MAX as u64),
        integer(
            "outputCapacity",
            "Rows buffered ahead of the query.",
            DEFAULT_OUTPUT_CAPACITY as u64,
        ),
        integer(
            "shutdownTimeoutMs",
            "Bound for stopping the snapshot worker.",
            DEFAULT_SHUTDOWN_TIMEOUT_MS,
        ),
    ])
    .expect("constant SQLite file bootstrap configuration")
}

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new_with_bootstrap(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        Vec::new(),
        vec![
            Arc::new(FileBootstrapFactory),
            Arc::new(coordinated_factory()),
            Arc::new(shared_factory()),
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
            configuration: file_definition().schema(),
            source_progress: false,
            pairing: None,
        }
    }

    /// A quiescent, unpaired provider in its served source's namespace whose
    /// element identities (tables and keys) must equal that source's.
    fn describe(&self, configuration: &serde_json::Value) -> Result<BootstrapOutputDescription> {
        let config = parse(configuration)?;
        Ok(BootstrapOutputDescription {
            completion: BootstrapCompletionKind::Quiescent,
            pairing: None,
            namespace: ElementNamespace::ServedSource,
            identity: Some(drasi_sqlite_common::native::element_identity(
                &config.tables,
                &config.table_keys(),
            )?),
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
            "file bootstrap does not bind persistent source progress"
        );
        let config = parse(&request.configuration)?;
        let settings = config.settings();
        let scope = request
            .scope
            .clone()
            .context("SQLite file bootstrap requires query scope")?;
        drasi_computation_contracts::validate_identifier("instance", &scope.instance_id)?;
        drasi_computation_contracts::validate_identifier("graph", &scope.graph_id)?;
        let served = session.served()?;
        let mut provider =
            SqliteBootstrapProvider::new(served.source.clone(), served.stream.clone(), settings)?
                .with_labels(session.labels.clone());
        provider.query_scope = Some((scope, request.id.clone()));
        Ok(NativeBootstrapProvider::Quiescent(Arc::new(provider)))
    }
}

fn parse(configuration: &serde_json::Value) -> Result<Configuration> {
    FileBootstrapFactory
        .metadata()
        .configuration
        .validate(configuration)?;
    file_definition().details().validate(configuration)?;
    Ok(serde_json::from_value(configuration.clone())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::{ComponentId, StreamId};
    use drasi_computation_plugin_sdk::{ExportedPlugin, Scope, ServedSource};
    use serde_json::json;

    const SOURCE_ID: &str = "052ef9a0-75ff-5510-bd5a-45b21f5f6ccd";

    fn session() -> Result<BootstrapSession> {
        Ok(BootstrapSession::new(
            Some(ServedSource {
                source: ComponentId::try_new(SOURCE_ID)?,
                stream: StreamId::try_new("source/out")?,
            }),
            None,
        ))
    }

    fn create(request: &CreateRequest) -> Result<NativeBootstrapProvider> {
        FileBootstrapFactory.create(request, &session()?, None)
    }

    fn request() -> Result<CreateRequest> {
        Ok(CreateRequest {
            inputs: Vec::new(),
            activation_credentials_bound: false,
            update_directive: None,
            id: ComponentId::try_new("c6de0d72-9721-581a-9dad-f91259a2ad07")?,
            implementation: FileBootstrapFactory.metadata().implementation,
            configuration_version: 1,
            scope: Some(Scope {
                instance_id: "instance".into(),
                graph_id: "graph".into(),
                generation: 1,
            }),
            configuration: json!({
                "path": "a-file-that-must-not-be-opened-at-construction.db",
                "maxTables": 16,
                "maxBytes": 65536,
                "maxDurationMs": 1000,
                "outputCapacity": 1,
                "shutdownTimeoutMs": 1000
            }),
        })
    }

    #[test]
    fn native_factory_is_configuration_only_and_query_scoped() -> Result<()> {
        ExportedPlugin::new(plugin()?)?;
        let request = request()?;
        let provider = create(&request)?;
        provider.validate_query_scope("instance", "graph", &request.id)?;
        assert!(provider
            .validate_query_scope("another", "graph", &request.id)
            .is_err());
        assert!(provider
            .validate_query_scope("instance", "another", &request.id)
            .is_err());
        assert!(provider
            .validate_query_scope(
                "instance",
                "graph",
                &ComponentId::try_new("71abd8b9-3bf1-55bb-9d8d-189a533e0ce0")?
            )
            .is_err());
        assert!(!FileBootstrapFactory.metadata().source_progress);
        Ok(())
    }

    #[test]
    fn minimal_configuration_uses_published_defaults() -> Result<()> {
        let mut request = request()?;
        request.configuration = json!({
            "path": "a-file-that-must-not-be-opened-at-construction.db",
        });
        create(&request)?;
        let details = file_definition().details();
        details.validate_schema(&FileBootstrapFactory.metadata().configuration)?;
        let parsed: Configuration = serde_json::from_value(request.configuration)?;
        assert_eq!(
            details.fields["maxTables"].default,
            Some(json!(parsed.max_tables.get()))
        );
        assert_eq!(
            details.fields["maxBytes"].default,
            Some(json!(parsed.max_bytes.get()))
        );
        assert_eq!(
            details.fields["maxDurationMs"].default,
            Some(json!(parsed.max_duration_ms.get()))
        );
        assert_eq!(
            details.fields["outputCapacity"].default,
            Some(json!(parsed.output_capacity.get()))
        );
        assert_eq!(
            details.fields["shutdownTimeoutMs"].default,
            Some(json!(parsed.shutdown_timeout_ms.get()))
        );
        Ok(())
    }

    #[test]
    fn native_factory_rejects_unknown_options_invalid_bounds_and_absent_scope() -> Result<()> {
        for field in [
            "unknown", "scope", "source", "stream", "bounds", "nested", "snake",
        ] {
            let mut request = request()?;
            match field {
                "unknown" => request.configuration["extra"] = json!(true),
                "scope" => request.scope = None,
                // The served source and stream come from the graph relation.
                "source" => request.configuration["source"] = json!(SOURCE_ID),
                "stream" => request.configuration["stream"] = json!("source/out"),
                "bounds" => request.configuration["outputCapacity"] = json!(0),
                "nested" => request.configuration["settings"] = json!({}),
                "snake" => request.configuration["max_tables"] = json!(4),
                _ => unreachable!(),
            }
            assert!(create(&request).is_err(), "{field}");
        }
        Ok(())
    }

    #[test]
    fn file_snapshot_declares_identities_from_its_tables_and_keys() -> Result<()> {
        let mut configuration = request()?.configuration;
        configuration["tables"] = json!(["sensors"]);
        configuration["tableKeys"] = json!([{"table": "sensors", "keyColumns": ["serial"]}]);
        let description = FileBootstrapFactory.describe(&configuration)?;
        assert_eq!(description.namespace, ElementNamespace::ServedSource);
        assert_eq!(description.completion, BootstrapCompletionKind::Quiescent);
        let keyed = description.identity.expect("declared identity");
        // A source with the same selection declares an equal identity; one
        // that identifies sensors by rowid does not.
        let source = drasi_sqlite_common::native::element_identity(
            &["sensors".into()],
            &[TableKeyConfig {
                table: "sensors".into(),
                key_columns: vec!["serial".into()],
            }],
        )?;
        assert_eq!(keyed, source);
        let rowid = drasi_sqlite_common::native::element_identity(&["sensors".into()], &[])?;
        assert_ne!(keyed, rowid);
        Ok(())
    }

    #[test]
    fn served_source_is_required_from_the_relation() -> Result<()> {
        let request = request()?;
        assert!(FileBootstrapFactory
            .create(&request, &BootstrapSession::default(), None)
            .is_err());
        Ok(())
    }

    #[test]
    fn coordinated_factories_take_no_configuration_and_require_their_protocol() -> Result<()> {
        for (factory, shared) in [(coordinated_factory(), false), (shared_factory(), true)] {
            let metadata = factory.metadata();
            assert_eq!(
                metadata.pairing,
                Some(drasi_sqlite_common::native::coordination_protocol(shared))
            );
            assert!(metadata.source_progress);
            assert!(metadata.configuration.fields.is_empty());
            let description = factory.describe(&json!({}))?;
            metadata.validate_description(&description)?;
            assert!(factory
                .describe(&json!({"source": SOURCE_ID, "stream": "source/out"}))
                .is_err());
        }
        Ok(())
    }
}
