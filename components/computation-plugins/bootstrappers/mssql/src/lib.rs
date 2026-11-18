// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native SQL Server bootstrap providers.
//!
//! - `drasi.mssql/bootstrap` pairs with a `drasi.mssql/changes` source that
//!   owns a `barrierTable`, through its declared coordination protocol. The
//!   source's coordinator opens each provider from the source's own settings,
//!   tables, keys and barrier table, so the provider takes no configuration
//!   and its markers are the ones the source erases.
//! - `drasi.mssql/snapshot` is a read-only snapshot in the namespace of the
//!   source it is connected to, with the same flat settings as the source
//!   (no `barrierTable`) plus `labels` and `stopTimeoutMs`.
//!
//! Every provider reads only the tables whose label a query declares.

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentId, ComputationBootstrapSnapshot, GraphChangeCodec, ImplementationIdentity,
};
use drasi_computation_plugin_sdk::{
    BootstrapCompletionKind, BootstrapFactory, BootstrapFactoryMetadata,
    BootstrapOutputDescription, BootstrapSession, ConfigDefinition, ConfigOption, ConfigType,
    CreateRequest, ElementNamespace, NativeBootstrapProvider, NativeSourceProgress,
    PairedBootstrapFactory, PluginDefinition, QuiescentBootstrapProvider, Scope,
};
use drasi_mssql_common::native::{
    coordination, definition,
    provider::{SnapshotProvider, DEFAULT_STOP_TIMEOUT_MS},
    NativeSettings,
};
use std::sync::Arc;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const MSSQL_BOOTSTRAP: &str = "drasi.mssql/bootstrap";
pub const MSSQL_SNAPSHOT: &str = "drasi.mssql/snapshot";
pub struct MsSqlReadOnlySnapshotFactory;

/// Upper bound on `stopTimeoutMs`.
pub const MAX_STOP_TIMEOUT_MS: u64 = 3_600_000;

/// The read-only snapshot's configuration: optional `labels` (an upper bound
/// on the tables read, each query's labels narrowing it) and `stopTimeoutMs`
/// beside the same flat settings the native source accepts. The served
/// source and stream come from the graph relation. Only `password` is secret.
#[derive(Clone, Debug)]
pub struct Configuration {
    pub labels: Vec<String>,
    /// How long `stop` waits for snapshot cleanup.
    pub stop_timeout_ms: u64,
    pub settings: NativeSettings,
}

impl Configuration {
    pub fn from_value(value: &serde_json::Value) -> Result<Self> {
        let mut object = value
            .as_object()
            .context("SQL Server bootstrap configuration must be an object")?
            .clone();
        let labels = match object.remove("labels") {
            None => Vec::new(),
            Some(value) => serde_json::from_value(value).context("labels must be strings")?,
        };
        let stop_timeout_ms = match object.remove("stopTimeoutMs") {
            None => DEFAULT_STOP_TIMEOUT_MS,
            Some(value) => value
                .as_u64()
                .filter(|ms| (1..=MAX_STOP_TIMEOUT_MS).contains(ms))
                .context("stopTimeoutMs must be an integer in 1..=3600000")?,
        };
        let settings = NativeSettings::from_value(serde_json::Value::Object(object))?;
        Ok(Self {
            labels,
            stop_timeout_ms,
            settings,
        })
    }
}

/// The read-only snapshot's published configuration: `labels` and
/// `stopTimeoutMs` beside the source's own settings options (from the shared
/// `drasi_mssql_common::native::definition`), without the source-only
/// `barrierTable`.
pub fn definition() -> ConfigDefinition {
    let mut options = vec![
        ConfigOption::new("labels", ConfigType::Array, false, "Upper bound on the tables read; each query's labels narrow it further. Empty reads every configured table.")
            .default(serde_json::json!([])),
        ConfigOption::new("stopTimeoutMs", ConfigType::Integer, false, "How long stop waits for snapshot cleanup.")
            .default(DEFAULT_STOP_TIMEOUT_MS)
            .minimum(1)
            .maximum(MAX_STOP_TIMEOUT_MS),
    ];
    options.extend(
        definition::options()
            .into_iter()
            .filter(|option| option.name() != "barrierTable"),
    );
    ConfigDefinition::try_new(options).expect("constant SQL Server snapshot configuration")
}

fn snapshot_metadata() -> BootstrapFactoryMetadata {
    BootstrapFactoryMetadata {
        implementation: ImplementationIdentity::try_new(MSSQL_SNAPSHOT, "1")
            .expect("constant implementation"),
        configuration_version: 1,
        configuration: definition().schema(),
        source_progress: false,
        pairing: None,
    }
}

/// Paired with a `drasi.mssql/changes` source that owns a `barrierTable`.
pub fn coordinated_factory() -> PairedBootstrapFactory {
    PairedBootstrapFactory::new(
        ImplementationIdentity::try_new(MSSQL_BOOTSTRAP, "1").expect("constant implementation"),
        coordination::protocol(),
    )
}

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new_with_bootstrap(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        Vec::new(),
        vec![
            Arc::new(coordinated_factory()),
            Arc::new(MsSqlReadOnlySnapshotFactory),
        ],
        vec![GraphChangeCodec::schema()],
    )
}

impl BootstrapFactory for MsSqlReadOnlySnapshotFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        snapshot_metadata()
    }
    fn describe(&self, configuration: &serde_json::Value) -> Result<BootstrapOutputDescription> {
        self.metadata().configuration.validate(configuration)?;
        Configuration::from_value(configuration)?;
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
        ensure!(
            progress.is_none(),
            "standalone SQL Server snapshots do not own CDC progress"
        );
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config = Configuration::from_value(&request.configuration)?;
        let served = session.served()?;
        let inner = SnapshotProvider::read_only(
            served.source.clone(),
            served.stream.clone(),
            config.settings,
            config.labels,
        )?
        .with_labels(session.labels.clone())
        .with_stop_timeout_ms(config.stop_timeout_ms);
        Ok(NativeBootstrapProvider::Quiescent(Arc::new(
            MsSqlReadOnlySnapshot {
                inner,
                scope: (
                    request
                        .scope
                        .clone()
                        .context("SQL Server snapshot requires query scope")?,
                    request.id.clone(),
                ),
            },
        )))
    }
}

/// A query's read-only SQL Server snapshot.
pub struct MsSqlReadOnlySnapshot {
    inner: SnapshotProvider,
    scope: (Scope, ComponentId),
}

#[async_trait]
impl QuiescentBootstrapProvider for MsSqlReadOnlySnapshot {
    fn validate_query_scope(&self, instance: &str, graph: &str, query: &ComponentId) -> Result<()> {
        let (scope, id) = &self.scope;
        ensure!(
            scope.instance_id == instance && scope.graph_id == graph && id == query,
            "SQL Server snapshot query scope mismatch"
        );
        Ok(())
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        self.inner.snapshot().await
    }
    async fn stop(&self) -> Result<()> {
        self.inner.stop().await
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::StreamId;
    use drasi_computation_plugin_sdk::ServedSource;

    const SOURCE_ID: &str = "129871df-b1dc-5c0d-aaec-9da46eb3f6b8";

    #[test]
    fn coordinated_provider_is_paired_and_takes_no_configuration() -> Result<()> {
        drasi_computation_plugin_sdk::ExportedPlugin::new(plugin()?)?;
        let factory = coordinated_factory();
        let metadata = factory.metadata();
        assert!(metadata.source_progress);
        assert_eq!(metadata.pairing, Some(coordination::protocol()));
        assert!(
            metadata.configuration.fields.is_empty(),
            "the source owns its settings and barrier table"
        );
        assert!(factory
            .describe(&serde_json::json!({"barrierTable": "dbo.Barrier"}))
            .is_err());
        assert!(!MsSqlReadOnlySnapshotFactory.metadata().source_progress);
        Ok(())
    }

    fn value() -> serde_json::Value {
        serde_json::json!({
            "host": "no-constructor-io", "database": "d", "user": "reader", "password": "p",
            "tables": ["dbo.Items"]
        })
    }

    fn request(configuration: serde_json::Value) -> Result<CreateRequest> {
        Ok(CreateRequest {
            inputs: Vec::new(),
            activation_credentials_bound: false,
            update_directive: None,
            id: ComponentId::try_new("c3f6a5b3-08d4-5c1f-8f61-d10a5e0de2f8")?,
            implementation: MsSqlReadOnlySnapshotFactory.metadata().implementation,
            configuration_version: 1,
            scope: Some(Scope {
                instance_id: "instance".into(),
                graph_id: "graph".into(),
                generation: 1,
            }),
            configuration,
        })
    }

    fn served() -> Result<BootstrapSession> {
        Ok(BootstrapSession::new(
            Some(ServedSource {
                source: ComponentId::try_new(SOURCE_ID)?,
                stream: StreamId::try_new("items/out")?,
            }),
            None,
        ))
    }

    #[test]
    fn standalone_mode_is_read_only_and_serves_its_relation_source() -> Result<()> {
        let provider = MsSqlReadOnlySnapshotFactory.create(&request(value())?, &served()?, None)?;
        assert_eq!(provider.completion(), BootstrapCompletionKind::Quiescent);
        assert!(provider.recovery_reader().is_none());
        let mut barrier = value();
        barrier["barrierTable"] = serde_json::json!("dbo.Barrier");
        assert!(MsSqlReadOnlySnapshotFactory
            .create(&request(barrier)?, &served()?, None)
            .is_err());
        let mut empty = value();
        empty["tables"] = serde_json::json!([]);
        assert!(MsSqlReadOnlySnapshotFactory
            .create(&request(empty)?, &served()?, None)
            .is_err());
        assert!(MsSqlReadOnlySnapshotFactory
            .create(&request(value())?, &BootstrapSession::default(), None)
            .is_err());
        for field in ["source", "stream"] {
            let mut legacy = value();
            legacy[field] = serde_json::json!(SOURCE_ID);
            assert!(
                MsSqlReadOnlySnapshotFactory.describe(&legacy).is_err(),
                "{field} comes from the graph relation"
            );
        }
        Ok(())
    }

    #[test]
    fn configuration_is_flat_and_shares_the_source_settings() -> Result<()> {
        let metadata = MsSqlReadOnlySnapshotFactory.metadata();
        metadata.configuration.validate(&value())?;
        let secrets: Vec<_> = metadata
            .configuration
            .fields
            .iter()
            .filter(|(_, field)| field.secret)
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(secrets, ["password"], "password is the only secret");
        let source = ConfigDefinition::try_new(definition::options())?;
        for (name, field) in &source.schema().fields {
            if name == "barrierTable" {
                assert!(!metadata.configuration.fields.contains_key(name));
                continue;
            }
            assert_eq!(
                metadata
                    .configuration
                    .fields
                    .get(name)
                    .map(|f| (f.value_type, f.secret)),
                Some((field.value_type, field.secret)),
                "{name} is published exactly as the source publishes it"
            );
            assert_eq!(
                definition().details().fields.get(name).map(|f| &f.default),
                source.details().fields.get(name).map(|f| &f.default),
                "{name} has the source's default"
            );
        }
        drasi_computation_plugin_sdk::testing::ensure_camel_case_configuration(
            &metadata.configuration,
        )?;
        let mut nested = value();
        nested["tableKeys"] = serde_json::json!([{"table": "dbo.Items", "key_columns": ["id"]}]);
        assert!(
            Configuration::from_value(&nested).is_err(),
            "nested keys are camelCase too"
        );
        let mut legacy = value();
        legacy["settings"] = serde_json::json!({"connection": {}});
        assert!(Configuration::from_value(&legacy).is_err());
        let mut unknown = value();
        unknown["pasword"] = serde_json::json!("x");
        assert!(Configuration::from_value(&unknown).is_err());
        let mut no_password = value();
        no_password.as_object_mut().unwrap().remove("password");
        assert!(
            MsSqlReadOnlySnapshotFactory
                .create(&request(no_password)?, &served()?, None)
                .is_err(),
            "standalone snapshots have no activation credentials"
        );
        Ok(())
    }
}
