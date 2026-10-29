// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Query-owned native MySQL initialization.

mod standalone;
#[cfg(test)]
mod tests;
pub use standalone::MySqlStandaloneBootstrapFactory;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_computation_contracts::{
    BootstrapPreparation, BootstrapState, BootstrapWatermark, ComponentId,
    ComputationBootstrapSnapshot, GraphChangeCodec, ImplementationIdentity, SourceProgressView,
    StreamId,
};
use drasi_computation_plugin_sdk::{
    BootstrapFactory, BootstrapFactoryMetadata, ConfigSchema, ConfigType, CreateRequest,
    NativeBootstrapProvider, NativeSourceProgress, PluginDefinition,
};
use drasi_mysql_common::native::{MySqlConfig, MySqlSnapshot};
use serde::Deserialize;
use std::sync::Arc;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const MYSQL_BOOTSTRAP: &str = "drasi.mysql/coordinated-bootstrap";
pub const MYSQL_SHARED_TRANSACTION_BOOTSTRAP: &str = "drasi.mysql/shared-transaction-bootstrap";
pub const MYSQL_SHARED_ROW_BOOTSTRAP: &str = "drasi.mysql/shared-row-bootstrap";
pub const MYSQL_STANDALONE_BOOTSTRAP: &str = "drasi.mysql/snapshot";

pub struct MySqlBootstrapFactory;
pub struct MySqlSharedTransactionBootstrapFactory;
pub struct MySqlSharedRowBootstrapFactory;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    source: String,
    stream: String,
    settings: MySqlConfig,
}

pub struct MySqlBootstrap {
    instance: String,
    inner: MySqlSnapshot,
}

#[async_trait]
impl NativeBootstrapProvider for MySqlBootstrap {
    fn validate_query_scope(&self, instance: &str, graph: &str, query: &ComponentId) -> Result<()> {
        anyhow::ensure!(
            self.instance == instance,
            "MySQL bootstrap belongs to another instance"
        );
        self.inner.validate_query_scope(instance, graph, query)
    }
    fn recovery_reader(&self) -> Option<SourceProgressView> {
        self.inner.recovery_reader()
    }
    async fn prepare(&self) -> Result<BootstrapPreparation> {
        anyhow::bail!("MySQL transaction bootstrap requires query-owned handover state")
    }
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        self.inner.prepare_with_state(state).await
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        self.inner.snapshot().await
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        self.inner.snapshot_with_state(state).await
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        self.inner.complete_snapshot().await
    }
    fn completion_state(&self) -> Result<Option<Bytes>> {
        self.inner.completion_state()
    }
    async fn stop(&self) -> Result<()> {
        self.inner.stop().await
    }
}

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new_with_bootstrap(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        Vec::new(),
        vec![
            Arc::new(MySqlBootstrapFactory),
            Arc::new(MySqlSharedTransactionBootstrapFactory),
            Arc::new(MySqlSharedRowBootstrapFactory),
            Arc::new(MySqlStandaloneBootstrapFactory),
        ],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

fn metadata(name: &str) -> BootstrapFactoryMetadata {
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
        implementation: ImplementationIdentity::try_new(name, "1").expect("implementation"),
        configuration_version: 1,
        configuration,
        source_progress: true,
    }
}

fn create(
    request: &CreateRequest,
    progress: Option<NativeSourceProgress>,
    shared: bool,
    rows: bool,
) -> Result<Arc<dyn NativeBootstrapProvider>> {
    let configuration: Configuration = serde_json::from_value(request.configuration.clone())?;
    let scope = request
        .scope
        .as_ref()
        .context("MySQL bootstrap requires query scope")?;
    ComponentId::try_new(scope.instance_id.as_str())?;
    ComponentId::try_new(scope.graph_id.as_str())?;
    let progress = progress
        .context("MySQL bootstrap requires its actual query progress owner")?
        .reader();
    anyhow::ensure!(
        progress.graph_id() == scope.graph_id && progress.component_id() == &request.id,
        "MySQL bootstrap progress belongs to another query"
    );
    let source = ComponentId::try_new(configuration.source)?;
    let stream = StreamId::try_new(configuration.stream)?;
    let inner = if rows {
        MySqlSnapshot::shared_rows(source, stream, configuration.settings, progress)?
    } else if shared {
        MySqlSnapshot::for_consumer(source, stream, configuration.settings, progress)?
    } else {
        MySqlSnapshot::new(source, stream, configuration.settings, progress)?
    };
    Ok(Arc::new(MySqlBootstrap {
        instance: scope.instance_id.clone(),
        inner,
    }))
}

impl BootstrapFactory for MySqlBootstrapFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        metadata(MYSQL_BOOTSTRAP)
    }
    fn create(
        &self,
        request: &CreateRequest,
        progress: Option<NativeSourceProgress>,
    ) -> Result<Arc<dyn NativeBootstrapProvider>> {
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        create(request, progress, false, false)
    }
}
impl BootstrapFactory for MySqlSharedTransactionBootstrapFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        metadata(MYSQL_SHARED_TRANSACTION_BOOTSTRAP)
    }
    fn create(
        &self,
        request: &CreateRequest,
        progress: Option<NativeSourceProgress>,
    ) -> Result<Arc<dyn NativeBootstrapProvider>> {
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        create(request, progress, true, false)
    }
}
impl BootstrapFactory for MySqlSharedRowBootstrapFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        metadata(MYSQL_SHARED_ROW_BOOTSTRAP)
    }
    fn create(
        &self,
        request: &CreateRequest,
        progress: Option<NativeSourceProgress>,
    ) -> Result<Arc<dyn NativeBootstrapProvider>> {
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        create(request, progress, true, true)
    }
}
