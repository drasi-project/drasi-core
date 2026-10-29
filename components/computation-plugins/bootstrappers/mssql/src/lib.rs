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
use drasi_mssql_common::native::{self, Cursor, NativeConfig, SnapshotRun};
use serde::{Deserialize, Serialize};
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

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub source: String,
    pub stream: String,
    pub settings: NativeConfig,
    #[serde(default)]
    pub labels: Vec<String>,
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
        let mut configuration = ConfigSchema::from_fields([
            ("source", ConfigType::String, true),
            ("stream", ConfigType::String, true),
            ("settings", ConfigType::Object, true),
            ("labels", ConfigType::Array, false),
        ]);
        configuration
            .fields
            .get_mut("settings")
            .expect("settings")
            .secret = true;
        BootstrapFactoryMetadata {
            implementation: ImplementationIdentity::try_new(MSSQL_BOOTSTRAP, "1")
                .expect("implementation"),
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
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let configuration: Configuration = serde_json::from_value(request.configuration.clone())?;
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
        let mut metadata = MsSqlBootstrapFactory.metadata();
        metadata.implementation =
            ImplementationIdentity::try_new(MSSQL_SNAPSHOT, "1").expect("implementation");
        metadata.source_progress = false;
        metadata
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
        let config: Configuration = serde_json::from_value(request.configuration.clone())?;
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
        if progress.is_some() {
            ensure!(
                config.settings.barrier_table.is_some(),
                "coordinated SQL Server bootstrap requires pre-provisioned barrier_table"
            );
        } else {
            ensure!(
                config.settings.barrier_table.is_none()
                    && !config.settings.connection.tables.is_empty(),
                "read-only SQL Server snapshot requires explicit tables and no barrier_table"
            );
        }
        ComponentId::try_new(config.source.as_str())?;
        StreamId::try_new(config.stream.as_str())?;
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
        let (run, rows) = native::start_snapshot(
            self.config.settings.clone(),
            self.config.source.clone(),
            self.config.labels.clone(),
            self.progress.is_some(),
        )?;
        *current = Some(run.clone());
        drop(current);
        let stream = StreamId::try_new(self.config.stream.as_str())?;
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
                    stream: StreamId::try_new(self.config.stream.as_str())?,
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
            run.stop(self.config.settings.shutdown_timeout()).await?;
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
    #[test]
    fn factory_exports_query_owned_consistent_bootstrap() -> Result<()> {
        drasi_computation_plugin_sdk::ExportedPlugin::new(plugin()?)?;
        assert!(MsSqlBootstrapFactory.metadata().source_progress);
        assert!(!MsSqlReadOnlySnapshotFactory.metadata().source_progress);
        Ok(())
    }

    #[test]
    fn standalone_mode_is_read_only_and_does_not_claim_cdc_progress() -> Result<()> {
        let config: Configuration = serde_json::from_value(serde_json::json!({
            "source":"items","stream":"items/out","settings":{
                "connection":{"host":"no-constructor-io","database":"d","user":"reader","tables":["dbo.Items"]}
            }
        }))?;
        let provider = MsSqlBootstrap::read_only(config.clone())?;
        assert!(provider.recovery_reader().is_none());
        let mut marker = config.clone();
        marker.settings.barrier_table = Some("dbo.Barrier".into());
        assert!(MsSqlBootstrap::read_only(marker).is_err());
        let mut empty = config;
        empty.settings.connection.tables.clear();
        assert!(MsSqlBootstrap::read_only(empty).is_err());
        Ok(())
    }
}
