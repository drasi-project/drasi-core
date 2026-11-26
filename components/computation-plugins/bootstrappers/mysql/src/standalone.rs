// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core_types::interface::FailureSurvival;
use drasi_mysql_common::native::standalone_snapshot::StandaloneSnapshot;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct MySqlStandaloneBootstrapFactory;

/// The standalone snapshot is not quiescent: a volatile query that completed
/// its snapshot refreshes it on restart (the live source does not replay what
/// it missed), so it declares a durable completion. It never returns a
/// watermark or completion state.
struct Provider {
    instance: String,
    graph: String,
    query: ComponentId,
    inner: StandaloneSnapshot,
    refresh_pending: AtomicBool,
}

#[async_trait]
impl DurableBootstrapProvider for Provider {
    fn validate_query_scope(&self, instance: &str, graph: &str, query: &ComponentId) -> Result<()> {
        anyhow::ensure!(
            instance == self.instance && graph == self.graph && query == &self.query,
            "MySQL standalone snapshot belongs to another query"
        );
        Ok(())
    }
    fn recovery_reader(&self) -> Option<SourceProgressView> {
        None
    }
    async fn prepare(&self, state: Option<&dyn BootstrapState>) -> Result<BootstrapPreparation> {
        self.inner.prepare().await?;
        let Some(state) = state else {
            return Ok(BootstrapPreparation::Ready);
        };
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
    fn has_pending_snapshot(&self) -> Result<bool> {
        Ok(false)
    }
    async fn snapshot(
        &self,
        _: Option<&dyn BootstrapState>,
    ) -> Result<ComputationBootstrapSnapshot> {
        self.inner.snapshot().await
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        self.inner.complete().await?;
        Ok(Vec::new())
    }
    fn completion_state(&self) -> Result<Option<bytes::Bytes>> {
        Ok(None)
    }
    async fn stop(&self) -> Result<()> {
        self.inner.stop().await
    }
}

impl BootstrapFactory for MySqlStandaloneBootstrapFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        BootstrapFactoryMetadata {
            implementation: ImplementationIdentity::try_new(MYSQL_STANDALONE_BOOTSTRAP, "1")
                .expect("implementation"),
            configuration_version: 1,
            configuration: config::definition().schema(),
            source_progress: false,
            independent_state: false,
            pairing: None,
        }
    }
    fn describe(&self, configuration: &serde_json::Value) -> Result<BootstrapOutputDescription> {
        self.metadata().configuration.validate(configuration)?;
        BootstrapConfig::from_value(configuration)?;
        Ok(BootstrapOutputDescription {
            completion: BootstrapCompletionKind::Durable,
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
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        anyhow::ensure!(
            progress.is_none(),
            "standalone MySQL snapshot has no recovery owner"
        );
        let config = BootstrapConfig::from_value(&request.configuration)?;
        let served = session.served()?;
        let settings = config.engine_config(&served.source)?;
        let scope = request
            .scope
            .as_ref()
            .context("MySQL standalone snapshot requires query scope")?;
        drasi_computation_contracts::validate_identifier("instance", &scope.instance_id)?;
        drasi_computation_contracts::validate_identifier("graph", &scope.graph_id)?;
        Ok(NativeBootstrapProvider::Durable(Arc::new(Provider {
            instance: scope.instance_id.clone(),
            graph: scope.graph_id.clone(),
            query: request.id.clone(),
            refresh_pending: AtomicBool::new(false),
            inner: StandaloneSnapshot::new(served.source.clone(), served.stream.clone(), settings)?
                .for_labels(&config.labels)
                .for_request(session.labels.as_ref()),
        })))
    }
}
