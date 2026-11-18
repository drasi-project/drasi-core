// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native PostgreSQL bootstrap providers.
//!
//! - `drasi.postgres/snapshot` is a standalone, read-only snapshot. It serves
//!   the source whose bootstrap input it is connected to (that source's
//!   namespace and stream, from the graph relation) and declares a quiescent
//!   completion.
//! - The coordinated and shared providers pair with the native PostgreSQL
//!   source through its declared coordination protocol. The source's
//!   coordinator creates their providers from the source's own identity,
//!   stream and settings (see `drasi_computation_source_postgres::bootstrap`),
//!   so they take no configuration and can only create slots under that
//!   source's managed prefix.
//!
//! Every provider reads only the tables whose label the query declares.

mod standalone;

use anyhow::{Context, Result};
use drasi_computation_contracts::{GraphChangeCodec, ImplementationIdentity};
use drasi_computation_plugin_sdk::{
    BootstrapCompletionKind, BootstrapFactory, BootstrapFactoryMetadata,
    BootstrapOutputDescription, BootstrapSession, CreateRequest, ElementNamespace,
    NativeBootstrapProvider, NativeSourceProgress, PairedBootstrapFactory, PluginDefinition,
};
use drasi_postgres_common::replication::coordination;
use std::sync::Arc;

pub use standalone::{definition, PostgresBootstrapConfig, PostgresBootstrapProvider};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const POSTGRES_BOOTSTRAP: &str = "drasi.postgres/coordinated-bootstrap";
pub const POSTGRES_SNAPSHOT: &str = "drasi.postgres/snapshot";
pub const POSTGRES_ROW_BOOTSTRAP: &str = "drasi.postgres/coordinated-row-bootstrap";
pub const POSTGRES_SHARED_ROW_BOOTSTRAP: &str = "drasi.postgres/shared-row-bootstrap";
pub const POSTGRES_SHARED_TRANSACTION_BOOTSTRAP: &str =
    "drasi.postgres/shared-transaction-bootstrap";

pub struct StandalonePostgresBootstrapFactory;

fn paired(name: &str, protocol: &str) -> PairedBootstrapFactory {
    PairedBootstrapFactory::new(
        ImplementationIdentity::try_new(name, "1").expect("constant implementation"),
        coordination::protocol(protocol),
    )
}

/// Paired with `drasi/postgres-transactions` configured with
/// `coordinatedSnapshot: true`.
pub fn coordinated_factory() -> PairedBootstrapFactory {
    paired(POSTGRES_BOOTSTRAP, coordination::COORDINATED_TRANSACTIONS)
}

/// Paired with `drasi.postgres/changes` configured with
/// `coordinatedSnapshot: true`.
pub fn coordinated_row_factory() -> PairedBootstrapFactory {
    paired(POSTGRES_ROW_BOOTSTRAP, coordination::COORDINATED_ROWS)
}

/// Paired with `drasi.postgres/shared-changes`.
pub fn shared_row_factory() -> PairedBootstrapFactory {
    paired(POSTGRES_SHARED_ROW_BOOTSTRAP, coordination::SHARED_ROWS)
}

/// Paired with `drasi.postgres/shared-transactions`.
pub fn shared_transaction_factory() -> PairedBootstrapFactory {
    paired(
        POSTGRES_SHARED_TRANSACTION_BOOTSTRAP,
        coordination::SHARED_TRANSACTIONS,
    )
}

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new_with_bootstrap(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        Vec::new(),
        vec![
            Arc::new(coordinated_factory()),
            Arc::new(StandalonePostgresBootstrapFactory),
            Arc::new(coordinated_row_factory()),
            Arc::new(shared_row_factory()),
            Arc::new(shared_transaction_factory()),
        ],
        vec![GraphChangeCodec::schema()],
    )
}

impl BootstrapFactory for StandalonePostgresBootstrapFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        BootstrapFactoryMetadata {
            implementation: ImplementationIdentity::try_new(POSTGRES_SNAPSHOT, "1")
                .expect("constant implementation"),
            configuration_version: 1,
            configuration: standalone::definition().schema(),
            source_progress: false,
            pairing: None,
        }
    }

    /// Quiescent, unpaired, in the served source's namespace.
    fn describe(&self, configuration: &serde_json::Value) -> Result<BootstrapOutputDescription> {
        self.metadata().configuration.validate(configuration)?;
        PostgresBootstrapConfig::from_value(configuration)?;
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
            "standalone PostgreSQL snapshots do not own source progress"
        );
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config = PostgresBootstrapConfig::from_value(&request.configuration)?;
        let scope = request
            .scope
            .clone()
            .context("PostgreSQL snapshot requires query scope")?;
        drasi_computation_contracts::validate_identifier("instance", &scope.instance_id)?;
        drasi_computation_contracts::validate_identifier("graph", &scope.graph_id)?;
        let served = session.served()?;
        let mut provider =
            PostgresBootstrapProvider::new(served.source.clone(), served.stream.clone(), config)?
                .with_labels(session.labels.clone());
        provider.query_scope = Some((scope, request.id.clone()));
        Ok(NativeBootstrapProvider::Quiescent(Arc::new(provider)))
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::{ComponentId, StreamId};
    use drasi_computation_plugin_sdk::{ExportedPlugin, Scope, ServedSource};

    const SOURCE_ID: &str = "ca61606a-85bf-514b-83b4-bdf81b539289";

    #[test]
    fn coordinated_factories_are_paired_and_take_no_settings() -> Result<()> {
        ExportedPlugin::new(plugin()?)?;
        for (factory, protocol) in [
            (
                coordinated_factory(),
                coordination::COORDINATED_TRANSACTIONS,
            ),
            (coordinated_row_factory(), coordination::COORDINATED_ROWS),
            (shared_row_factory(), coordination::SHARED_ROWS),
            (
                shared_transaction_factory(),
                coordination::SHARED_TRANSACTIONS,
            ),
        ] {
            let metadata = factory.metadata();
            assert!(metadata.source_progress);
            assert_eq!(metadata.pairing, Some(coordination::protocol(protocol)));
            assert!(
                metadata.configuration.fields.is_empty(),
                "the source owns the settings, slot prefix and table selection"
            );
            metadata.validate_description(&factory.describe(&serde_json::json!({}))?)?;
            assert!(factory
                .describe(&serde_json::json!({"source": SOURCE_ID, "stream": "items/out"}))
                .is_err());
        }
        let standalone = StandalonePostgresBootstrapFactory.metadata().configuration;
        let secrets: Vec<_> = standalone
            .fields
            .iter()
            .filter(|(_, field)| field.secret)
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(secrets, ["password"], "only the password is secret");
        Ok(())
    }

    fn request() -> Result<CreateRequest> {
        Ok(CreateRequest {
            inputs: Vec::new(),
            activation_credentials_bound: false,
            update_directive: None,
            id: ComponentId::try_new("5244cc6b-39d7-5693-be0e-6cbc7e8504ad")?,
            implementation: StandalonePostgresBootstrapFactory.metadata().implementation,
            configuration_version: 1,
            scope: Some(Scope {
                instance_id: "instance".into(),
                graph_id: "graph".into(),
                generation: 1,
            }),
            configuration: serde_json::json!({
                "host": "must-not-connect-at-construction", "database": "db", "user": "reader",
                "tables": ["items"], "maxTables": 8, "maxProtocolBytes": 65536,
                "ioTimeoutMs": 1000, "maxDurationMs": 10000
            }),
        })
    }

    #[test]
    fn standalone_factory_needs_no_replication_or_progress_and_is_configuration_only() -> Result<()>
    {
        let request = request()?;
        let session = BootstrapSession::new(
            Some(ServedSource {
                source: ComponentId::try_new(SOURCE_ID)?,
                stream: StreamId::try_new("seed/out")?,
            }),
            None,
        );
        let provider = StandalonePostgresBootstrapFactory.create(&request, &session, None)?;
        assert_eq!(provider.completion(), BootstrapCompletionKind::Quiescent);
        provider.validate_query_scope("instance", "graph", &request.id)?;
        assert!(provider.recovery_reader().is_none());
        assert!(provider
            .validate_query_scope("other", "graph", &request.id)
            .is_err());
        assert!(
            StandalonePostgresBootstrapFactory
                .create(&request, &BootstrapSession::default(), None)
                .is_err(),
            "the served source comes from the graph relation"
        );
        let mut legacy = request.configuration.clone();
        legacy["source"] = serde_json::json!(SOURCE_ID);
        assert!(StandalonePostgresBootstrapFactory
            .describe(&legacy)
            .is_err());
        Ok(())
    }

    #[test]
    fn standalone_configuration_is_flat_camel_case_with_published_defaults() -> Result<()> {
        let definition = definition();
        let schema = definition.schema();
        drasi_computation_plugin_sdk::testing::ensure_camel_case_configuration(&schema)?;
        let minimal = serde_json::json!({"database": "db", "user": "reader", "tables": ["items"]});
        let parsed = serde_json::to_value(PostgresBootstrapConfig::from_value(&minimal)?)?;
        for (name, field) in definition.details().fields {
            if let Some(default) = field.default {
                assert_eq!(parsed.get(&name), Some(&default), "{name}");
            }
        }
        let shared = drasi_computation_plugin_sdk::ConfigDefinition::try_new(
            drasi_postgres_common::replication::definition::options(),
        )?
        .schema();
        for name in [
            "host",
            "port",
            "sslMode",
            "tlsCaPem",
            "tableKeys",
            "password",
        ] {
            assert_eq!(
                schema.fields[name].secret, shared.fields[name].secret,
                "{name} is published as the source publishes it"
            );
        }
        for (key, value) in [
            ("settings", serde_json::json!({})),
            ("max_tables", serde_json::json!(4)),
            ("ssl_mode", serde_json::json!("disable")),
            (
                "tableKeys",
                serde_json::json!([{"table": "items", "key_columns": ["id"]}]),
            ),
        ] {
            let mut config = minimal.clone();
            config[key] = value;
            assert!(
                StandalonePostgresBootstrapFactory
                    .describe(&config)
                    .is_err(),
                "{key} is refused"
            );
        }
        Ok(())
    }
}
