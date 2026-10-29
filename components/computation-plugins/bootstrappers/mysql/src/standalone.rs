// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core_types::interface::FailureSurvival;
use drasi_mysql_common::native::standalone_snapshot::StandaloneSnapshot;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct MySqlStandaloneBootstrapFactory;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    source: String,
    stream: String,
    settings: MySqlConfig,
    #[serde(default)]
    labels: Vec<String>,
}

struct Provider {
    instance: String,
    graph: String,
    query: ComponentId,
    inner: StandaloneSnapshot,
    refresh_pending: AtomicBool,
}

#[async_trait]
impl NativeBootstrapProvider for Provider {
    fn validate_query_scope(&self, instance: &str, graph: &str, query: &ComponentId) -> Result<()> {
        anyhow::ensure!(
            instance == self.instance && graph == self.graph && query == &self.query,
            "MySQL standalone snapshot belongs to another query"
        );
        Ok(())
    }
    async fn prepare(&self) -> Result<BootstrapPreparation> {
        self.inner.prepare().await?;
        Ok(BootstrapPreparation::Ready)
    }
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        self.inner.prepare().await?;
        match state.durability().process_restart {
            FailureSurvival::Guaranteed => Ok(BootstrapPreparation::Ready),
            FailureSurvival::NotGuaranteed => {
                if self.inner.completed() && !self.refresh_pending.swap(true, Ordering::AcqRel) {
                    return Ok(BootstrapPreparation::RefreshVolatile);
                }
                self.refresh_pending.store(false, Ordering::Release);
                Ok(BootstrapPreparation::Ready)
            }
            FailureSurvival::Unknown => {
                anyhow::bail!("MySQL snapshot requires known query storage durability")
            }
        }
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        self.inner.snapshot().await
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        self.inner.complete().await?;
        Ok(Vec::new())
    }
    async fn stop(&self) -> Result<()> {
        self.inner.stop().await
    }
}

impl BootstrapFactory for MySqlStandaloneBootstrapFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        let mut metadata = metadata(MYSQL_STANDALONE_BOOTSTRAP);
        metadata.source_progress = false;
        metadata.configuration.fields.insert(
            "labels".into(),
            drasi_computation_plugin_sdk::ConfigField {
                value_type: ConfigType::Array,
                required: false,
                secret: false,
            },
        );
        metadata
    }
    fn create(
        &self,
        request: &CreateRequest,
        progress: Option<NativeSourceProgress>,
    ) -> Result<Arc<dyn NativeBootstrapProvider>> {
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        anyhow::ensure!(
            progress.is_none(),
            "standalone MySQL snapshot has no recovery owner"
        );
        let config: Configuration = serde_json::from_value(request.configuration.clone())?;
        let scope = request
            .scope
            .as_ref()
            .context("MySQL standalone snapshot requires query scope")?;
        ComponentId::try_new(scope.instance_id.as_str())?;
        ComponentId::try_new(scope.graph_id.as_str())?;
        Ok(Arc::new(Provider {
            instance: scope.instance_id.clone(),
            graph: scope.graph_id.clone(),
            query: request.id.clone(),
            refresh_pending: AtomicBool::new(false),
            inner: StandaloneSnapshot::new(
                ComponentId::try_new(config.source)?,
                StreamId::try_new(config.stream)?,
                config.settings,
            )?
            .for_labels(&config.labels),
        }))
    }
}
