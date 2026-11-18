// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Pairs a SQL Server source that owns a barrier table with its coordinated
//! bootstrap providers. The coordinator constructs the source and opens each
//! consumer's provider from the same request, so both use one identity,
//! stream, table selection, keys and barrier table: the source owns and
//! declares `barrierTable`, and erases the markers its providers write there.

use crate::{component::MsSqlSource, config::SourceConfig, tiberius_connectors};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    BootstrapCoordination, BootstrapLabels, BootstrapPreparation, BootstrapProtocol,
    BootstrapState, BootstrapWatermark, ComponentId, ComputationBootstrapSnapshot,
    SourceProgressView, StreamId,
};
use drasi_computation_plugin_sdk::{
    Component, ControlSender, CreateRequest, CreatedComponent, DurableBootstrapProvider,
    NativeSourceBootstrapCoordinator, SourceBootstrapLiveProgress, SourceBootstrapProgress,
};
use drasi_mssql_common::native::{
    coordination,
    provider::{CredentialSource, SnapshotProvider},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

/// The source's coordination offer: a source with a barrier table can hand
/// over gap-free to its paired providers, and still accepts an unpaired
/// snapshot.
pub fn offer(config: &SourceConfig) -> Option<BootstrapCoordination> {
    config
        .settings
        .barrier_table
        .as_ref()
        .map(|_| BootstrapCoordination {
            protocol: coordination::protocol(),
            required: false,
        })
}

/// Adds the configured source's coordination offer to its description.
pub(crate) fn declare(
    request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    description: drasi_computation_plugin_sdk::ComponentDescription,
) -> Result<drasi_computation_plugin_sdk::ComponentDescription> {
    let config = SourceConfig::from_value(&request.request.configuration)?;
    Ok(match offer(&config) {
        Some(offer) => description.with_coordination(offer),
        None => description,
    })
}

pub(crate) struct Coordinator {
    request: CreateRequest,
    config: SourceConfig,
    members: BTreeMap<ComponentId, SourceProgressView>,
    /// The constructed source's credentials, once constructed.
    credentials: Mutex<Option<CredentialSource>>,
}

impl Coordinator {
    pub(crate) fn new(
        request: &CreateRequest,
        config: SourceConfig,
        progress: SourceBootstrapProgress,
    ) -> Result<Self> {
        let scope = request
            .scope
            .as_ref()
            .context("SQL Server coordination requires graph scope")?;
        anyhow::ensure!(
            offer(&config).is_some(),
            "SQL Server source coordinates bootstrap only with a barrierTable"
        );
        let SourceBootstrapProgress::Shared { consumers, members } = progress else {
            anyhow::bail!("SQL Server coordination requires the source's consumer set");
        };
        anyhow::ensure!(
            consumers.graph_id() == scope.graph_id && consumers.producer_id() == &request.id,
            "SQL Server coordinator consumer set belongs to another graph or source"
        );
        Ok(Self {
            request: request.clone(),
            config,
            members,
            credentials: Mutex::new(None),
        })
    }

    fn validate_request(&self, request: &CreateRequest) -> Result<()> {
        let (expected, actual) = (
            self.request
                .scope
                .as_ref()
                .context("SQL Server coordinator scope is missing")?,
            request
                .scope
                .as_ref()
                .context("SQL Server source scope is missing")?,
        );
        anyhow::ensure!(
            request.id == self.request.id
                && request.implementation == self.request.implementation
                && request.configuration_version == self.request.configuration_version
                && request.configuration == self.request.configuration
                && request.activation_credentials_bound
                    == self.request.activation_credentials_bound
                && actual.instance_id == expected.instance_id
                && actual.graph_id == expected.graph_id,
            "SQL Server source does not match its actual coordinator binding"
        );
        Ok(())
    }
}

#[async_trait]
impl NativeSourceBootstrapCoordinator for Coordinator {
    fn protocol(&self) -> BootstrapProtocol {
        coordination::protocol()
    }

    fn create_source(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        live: SourceBootstrapLiveProgress,
    ) -> Result<CreatedComponent> {
        self.validate_request(request)?;
        let SourceBootstrapLiveProgress::Shared(reader) = live else {
            anyhow::bail!("SQL Server source/bootstrap live progress mode changed");
        };
        let members: Vec<_> = self.members.keys().cloned().collect();
        let source = MsSqlSource::new(
            request.id.clone(),
            self.config.clone(),
            reader,
            &members,
            request.activation_credentials_bound,
            tiberius_connectors(),
        )?;
        *self
            .credentials
            .lock()
            .map_err(|_| anyhow::anyhow!("SQL Server coordinator credentials poisoned"))? =
            Some(source.credential_source());
        Ok(Component::Source(Box::new(source)).into())
    }

    fn open(
        &self,
        query: &ComponentId,
        progress: SourceProgressView,
        labels: Option<&BootstrapLabels>,
    ) -> Result<Arc<dyn DurableBootstrapProvider>> {
        let expected = self
            .members
            .get(query)
            .context("query is not a SQL Server coordinator consumer")?;
        anyhow::ensure!(
            expected.same_owner(&progress),
            "SQL Server bootstrap requires its captured actual query reader"
        );
        // The constructed source's activation credentials, or its literal
        // login before construction when no identity is bound.
        let constructed = self
            .credentials
            .lock()
            .map_err(|_| anyhow::anyhow!("SQL Server coordinator credentials poisoned"))?
            .clone();
        let credentials: CredentialSource = match constructed {
            Some(credentials) => credentials,
            None => {
                anyhow::ensure!(
                    !self.request.activation_credentials_bound,
                    "SQL Server paired bootstrap authenticates with its source's activation \
                     identity, which exists only once the source is constructed"
                );
                let literal = self.config.settings.literal_authentication();
                Arc::new(move || Ok(literal.clone()))
            }
        };
        let inner = SnapshotProvider::coordinated(
            self.request.id.clone(),
            StreamId::try_new(self.config.stream.clone())?,
            self.config.settings.clone(),
            progress,
            credentials,
        )?
        .with_labels(labels.cloned());
        Ok(Arc::new(Provider { inner }))
    }

    /// Providers own their snapshot runs and are stopped individually.
    async fn stop(&self) -> Result<()> {
        Ok(())
    }
}

struct Provider {
    inner: SnapshotProvider,
}

#[async_trait]
impl DurableBootstrapProvider for Provider {
    fn validate_query_scope(
        &self,
        _instance: &str,
        graph: &str,
        query: &ComponentId,
    ) -> Result<()> {
        self.inner.validate_query(graph, query)
    }
    fn recovery_reader(&self) -> Option<SourceProgressView> {
        self.inner.recovery_reader()
    }
    async fn prepare(&self, _state: Option<&dyn BootstrapState>) -> Result<BootstrapPreparation> {
        self.inner.prepare()
    }
    fn has_pending_snapshot(&self) -> Result<bool> {
        Ok(false)
    }
    async fn snapshot(
        &self,
        _state: Option<&dyn BootstrapState>,
    ) -> Result<ComputationBootstrapSnapshot> {
        self.inner.snapshot().await
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        self.inner.complete_snapshot().await
    }
    /// The handover is the boundary watermark; there is no other state.
    fn completion_state(&self) -> Result<Option<bytes::Bytes>> {
        Ok(None)
    }
    async fn stop(&self) -> Result<()> {
        self.inner.stop().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_source_that_owns_a_barrier_table_offers_coordination() -> Result<()> {
        let value = serde_json::json!({
            "stream": "items/out", "host": "h", "database": "d", "user": "u", "password": "p",
            "tables": ["dbo.Items"]
        });
        assert_eq!(offer(&SourceConfig::from_value(&value)?), None);
        let mut barrier = value;
        barrier["barrierTable"] = serde_json::json!("dbo.DrasiBarrier");
        let offered = offer(&SourceConfig::from_value(&barrier)?).context("offered")?;
        assert_eq!(offered.protocol, coordination::protocol());
        assert!(!offered.required);
        Ok(())
    }
}
