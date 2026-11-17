// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    BootstrapPreparation, BootstrapWatermark, ComponentId, ComputationBootstrapSnapshot,
    GraphChangeCodec, ImplementationIdentity, SourceProgressKey, SourceProgressView, StreamId,
};
use drasi_computation_plugin_sdk::{
    BootstrapFactory, BootstrapFactoryMetadata, ConfigSchema, ConfigType, CreateRequest,
    NativeBootstrapProvider, NativeSourceProgress, PluginDefinition, Scope,
};
use drasi_mssql_common::native::{
    self, markers, settings::SETTINGS_FIELDS, Cursor, NativeSettings, SnapshotRun,
};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::Mutex;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const MSSQL_BOOTSTRAP: &str = "drasi.mssql/bootstrap";
pub const MSSQL_SNAPSHOT: &str = "drasi.mssql/snapshot";
pub struct MsSqlBootstrapFactory;
pub struct MsSqlReadOnlySnapshotFactory;

/// A provider's configuration: `source`, `stream` and optional `labels`
/// beside the same flat settings the native source accepts, so both select
/// the same tables and keys and compute one binding. Only `password` is
/// secret.
#[derive(Clone, Debug)]
pub struct Configuration {
    pub source: ComponentId,
    pub stream: StreamId,
    pub labels: Vec<String>,
    /// How long `stop` waits for snapshot cleanup (bootstrap only).
    pub stop_timeout_ms: u64,
    pub settings: NativeSettings,
}

/// Default `stopTimeoutMs`.
pub const DEFAULT_STOP_TIMEOUT_MS: u64 = 30_000;

impl Configuration {
    pub fn from_value(value: &serde_json::Value) -> Result<Self> {
        let mut object = value
            .as_object()
            .context("SQL Server bootstrap configuration must be an object")?
            .clone();
        let mut text = |name: &str| match object.remove(name) {
            Some(serde_json::Value::String(value)) => Ok(value),
            Some(_) => anyhow::bail!("{name} must be a string"),
            None => anyhow::bail!("{name} is required"),
        };
        let source = ComponentId::try_new(text("source")?)?;
        let stream = StreamId::try_new(text("stream")?)?;
        let labels = match object.remove("labels") {
            None => Vec::new(),
            Some(value) => serde_json::from_value(value).context("labels must be strings")?,
        };
        let stop_timeout_ms = match object.remove("stopTimeoutMs") {
            None => DEFAULT_STOP_TIMEOUT_MS,
            Some(value) => value
                .as_u64()
                .filter(|ms| (1..=3_600_000).contains(ms))
                .context("stopTimeoutMs must be an integer in 1..=3600000")?,
        };
        let settings = NativeSettings::from_value(serde_json::Value::Object(object))?;
        Ok(Self {
            source,
            stream,
            labels,
            stop_timeout_ms,
            settings,
        })
    }
}

fn metadata(implementation: &str, source_progress: bool) -> BootstrapFactoryMetadata {
    let mut fields = vec![
        ("source", ConfigType::ComponentId, true),
        ("stream", ConfigType::String, true),
        ("labels", ConfigType::Array, false),
        ("stopTimeoutMs", ConfigType::Integer, false),
    ];
    for name in SETTINGS_FIELDS {
        let kind = match *name {
            "port" | "connectTimeoutMs" | "commandTimeoutMs" | "pollIntervalMs" | "maxRows"
            | "maxBytes" | "reconnectBudgetMs" => ConfigType::Integer,
            "trustServerCertificate" => ConfigType::Boolean,
            "tables" | "tableKeys" => ConfigType::Array,
            _ => ConfigType::String,
        };
        fields.push((name, kind, *name == "database"));
    }
    let mut configuration = ConfigSchema::from_fields(fields);
    if let Some(password) = configuration.fields.get_mut("password") {
        password.secret = true;
    }
    BootstrapFactoryMetadata {
        implementation: ImplementationIdentity::try_new(implementation, "1")
            .expect("constant implementation"),
        configuration_version: 1,
        configuration,
        source_progress,
    }
}

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new_with_bootstrap(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        Vec::new(),
        vec![
            Arc::new(MsSqlBootstrapFactory),
            Arc::new(MsSqlReadOnlySnapshotFactory),
        ],
        vec![GraphChangeCodec::schema()],
    )
}
impl BootstrapFactory for MsSqlBootstrapFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        metadata(MSSQL_BOOTSTRAP, true)
    }
    fn create(
        &self,
        request: &CreateRequest,
        progress: Option<NativeSourceProgress>,
    ) -> Result<Arc<dyn NativeBootstrapProvider>> {
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let configuration = Configuration::from_value(&request.configuration)?;
        let progress = progress
            .context("SQL Server bootstrap requires query-owned progress")?
            .reader();
        let scope = request
            .scope
            .clone()
            .context("SQL Server bootstrap requires query scope")?;
        ensure!(
            progress.graph_id() == scope.graph_id && progress.component_id() == &request.id,
            "SQL Server bootstrap progress scope mismatch"
        );
        let mut provider = MsSqlBootstrap::new(configuration, progress)?;
        provider.scope = Some((scope, request.id.clone()));
        Ok(Arc::new(provider))
    }
}

impl BootstrapFactory for MsSqlReadOnlySnapshotFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        metadata(MSSQL_SNAPSHOT, false)
    }
    fn create(
        &self,
        request: &CreateRequest,
        progress: Option<NativeSourceProgress>,
    ) -> Result<Arc<dyn NativeBootstrapProvider>> {
        ensure!(
            progress.is_none(),
            "standalone SQL Server snapshots do not own CDC progress"
        );
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config = Configuration::from_value(&request.configuration)?;
        let mut provider = MsSqlBootstrap::read_only(config)?;
        provider.scope = Some((
            request
                .scope
                .clone()
                .context("SQL Server snapshot requires query scope")?,
            request.id.clone(),
        ));
        Ok(Arc::new(provider))
    }
}

pub struct MsSqlBootstrap {
    config: Configuration,
    progress: Option<SourceProgressView>,
    run: Mutex<Option<Arc<SnapshotRun>>>,
    generation: Arc<AtomicU64>,
    scope: Option<(Scope, ComponentId)>,
}
impl MsSqlBootstrap {
    pub fn new(config: Configuration, progress: SourceProgressView) -> Result<Self> {
        Self::construct(config, Some(progress))
    }
    pub fn read_only(config: Configuration) -> Result<Self> {
        Self::construct(config, None)
    }
    fn construct(config: Configuration, progress: Option<SourceProgressView>) -> Result<Self> {
        config.settings.validate()?;
        // Bootstrap factories receive no activation credentials.
        config.settings.validate_credentials(false)?;
        if progress.is_some() {
            ensure!(
                config.settings.barrier_table.is_some(),
                "coordinated SQL Server bootstrap requires a pre-provisioned barrierTable"
            );
        } else {
            ensure!(
                config.settings.barrier_table.is_none() && !config.settings.tables.is_empty(),
                "read-only SQL Server snapshot requires explicit tables and no barrierTable"
            );
        }
        Ok(Self {
            config,
            progress,
            run: Mutex::new(None),
            generation: Arc::default(),
            scope: None,
        })
    }
}
#[async_trait]
impl NativeBootstrapProvider for MsSqlBootstrap {
    fn recovery_reader(&self) -> Option<SourceProgressView> {
        self.progress.clone()
    }
    fn validate_query_scope(&self, instance: &str, graph: &str, query: &ComponentId) -> Result<()> {
        if let Some((scope, id)) = &self.scope {
            ensure!(
                scope.instance_id == instance && scope.graph_id == graph && id == query,
                "SQL Server bootstrap query scope mismatch"
            );
        }
        Ok(())
    }
    async fn prepare(&self) -> Result<BootstrapPreparation> {
        if let Some(progress) = &self.progress {
            let progress = progress.snapshot()?;
            if let Some(failure) = &progress.failure {
                anyhow::bail!("SQL Server bootstrap progress failed: {failure}");
            }
            if let Some(checkpoint) = progress
                .checkpoints
                .get(&SourceProgressKey::Source(self.config.source.clone()))
            {
                Cursor::decode(
                    checkpoint
                        .source_position
                        .as_deref()
                        .context("SQL Server checkpoint lacks CDC position")?,
                )?;
            }
        }
        Ok(BootstrapPreparation::Ready)
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        let mut current = self.run.lock().await;
        ensure!(
            current.is_none(),
            "SQL Server snapshot requires successful stop before reuse"
        );
        let generation = self.generation.load(Ordering::Acquire);
        let marker_prefix = match (&self.progress, &self.scope) {
            (Some(progress), _) => Some(markers::marker_prefix(
                progress.graph_id(),
                self.config.source.as_str(),
            )),
            (None, _) => None,
        };
        self.config
            .settings
            .connection()
            .warn_about_transport("Native SQL Server bootstrap");
        let (run, rows) = native::start_snapshot(
            self.config.settings.clone(),
            self.config.settings.literal_authentication(),
            self.config.source.to_string(),
            self.config.labels.clone(),
            marker_prefix,
        )?;
        *current = Some(run.clone());
        drop(current);
        let stream = self.config.stream.clone();
        let active = self.generation.clone();
        let changes = futures_util::stream::unfold(
            (rows, run, active, stream, 0u64, false),
            move |(mut rows, run, active, stream, sequence, failed)| async move {
                if failed {
                    return None;
                }
                if active.load(Ordering::Acquire) != generation {
                    run.cancel();
                    return Some((
                        Err(anyhow::anyhow!("SQL Server snapshot generation retired")),
                        (rows, run, active, stream, sequence, true),
                    ));
                }
                let next = rows.next().await;
                let encoded = match next {
                    Ok(None) => return None,
                    Ok(Some(change)) => {
                        if active.load(Ordering::Acquire) != generation {
                            Err(anyhow::anyhow!("SQL Server snapshot generation retired"))
                        } else {
                            sequence
                                .checked_add(1)
                                .context("SQL Server snapshot sequence exhausted")
                                .and_then(|sequence| {
                                    GraphChangeCodec::encode_change(
                                        change,
                                        stream.clone(),
                                        sequence,
                                        Some(chrono::Utc::now()),
                                    )
                                    .map_err(Into::into)
                                })
                        }
                    }
                    Err(error) => Err(error),
                };
                let failed = encoded.is_err();
                if failed {
                    run.cancel();
                }
                Some((
                    encoded,
                    (
                        rows,
                        run,
                        active,
                        stream,
                        sequence.saturating_add(1),
                        failed,
                    ),
                ))
            },
        );
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(changes),
            watermarks: Vec::new(),
        })
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        let run = self
            .run
            .lock()
            .await
            .clone()
            .context("SQL Server snapshot is not active")?;
        run.complete()
            .await?
            .map(|boundary| {
                Ok::<_, anyhow::Error>(BootstrapWatermark {
                    producer: None,
                    stream: self.config.stream.clone(),
                    source_id: Some(self.config.source.clone()),
                    sequence: 0,
                    position: Some(boundary.encode()?.into()),
                })
            })
            .transpose()
            .map(|watermark| watermark.into_iter().collect())
    }
    async fn stop(&self) -> Result<()> {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let run = self.run.lock().await.clone();
        if let Some(run) = run {
            run.stop(std::time::Duration::from_millis(
                self.config.stop_timeout_ms,
            ))
            .await?;
            let mut current = self.run.lock().await;
            if current
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &run))
            {
                *current = None;
            }
        }
        Ok(())
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE_ID: &str = "129871df-b1dc-5c0d-aaec-9da46eb3f6b8";

    #[test]
    fn factory_exports_query_owned_consistent_bootstrap() -> Result<()> {
        drasi_computation_plugin_sdk::ExportedPlugin::new(plugin()?)?;
        assert!(MsSqlBootstrapFactory.metadata().source_progress);
        assert!(!MsSqlReadOnlySnapshotFactory.metadata().source_progress);
        Ok(())
    }

    fn value() -> serde_json::Value {
        serde_json::json!({
            "source": SOURCE_ID, "stream": "items/out", "host": "no-constructor-io",
            "database": "d", "user": "reader", "password": "p", "tables": ["dbo.Items"]
        })
    }

    #[test]
    fn standalone_mode_is_read_only_and_does_not_claim_cdc_progress() -> Result<()> {
        let config = Configuration::from_value(&value())?;
        let provider = MsSqlBootstrap::read_only(config.clone())?;
        assert!(provider.recovery_reader().is_none());
        let mut marker = config.clone();
        marker.settings.barrier_table = Some("dbo.Barrier".into());
        assert!(MsSqlBootstrap::read_only(marker).is_err());
        let mut empty = config;
        empty.settings.tables.clear();
        assert!(MsSqlBootstrap::read_only(empty).is_err());
        Ok(())
    }

    #[test]
    fn configuration_is_flat_and_shares_the_source_settings() -> Result<()> {
        let metadata = MsSqlBootstrapFactory.metadata();
        metadata.configuration.validate(&value())?;
        let secrets: Vec<_> = metadata
            .configuration
            .fields
            .iter()
            .filter(|(_, field)| field.secret)
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(secrets, ["password"], "password is the only secret");
        for name in SETTINGS_FIELDS {
            assert!(metadata.configuration.fields.contains_key(*name), "{name}");
        }
        let mut legacy = value();
        legacy["settings"] = serde_json::json!({"connection": {}});
        assert!(Configuration::from_value(&legacy).is_err());
        let mut unknown = value();
        unknown["pasword"] = serde_json::json!("x");
        assert!(Configuration::from_value(&unknown).is_err());
        let mut no_password = value();
        no_password.as_object_mut().unwrap().remove("password");
        let config = Configuration::from_value(&no_password)?;
        assert!(
            MsSqlBootstrap::read_only(config).is_err(),
            "bootstrap providers have no activation credentials"
        );
        Ok(())
    }
}
