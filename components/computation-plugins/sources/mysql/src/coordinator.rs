// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Pairs a MySQL source configured with `startFrom: snapshot` with its
//! coordinated bootstrap providers. The coordinator constructs the source and
//! opens each query's provider from the same request, so both use one
//! identity, stream, server id, table selection, keys and retention; the
//! provider's configuration repeats none of them.

use crate::{
    component::MySqlComponent,
    config::{Mode, SourceConfig},
    status_sink,
};
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
use drasi_mysql_common::native::{
    coordination, progress::ConsumerBinding, rows, ActivationCredentials, MySqlSnapshot,
    MySqlSource, StartFrom,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

/// The coordination protocol of a source in `mode`, if the mode can pair.
pub fn protocol(mode: Mode) -> Option<BootstrapProtocol> {
    Some(coordination::protocol(match mode {
        Mode::Transactions => coordination::TRANSACTIONS,
        Mode::SharedTransactions => coordination::SHARED_TRANSACTIONS,
        Mode::SharedChanges => coordination::SHARED_ROWS,
        _ => return None,
    }))
}

/// The source's coordination offer: a tracked source starting from a
/// snapshot requires its paired provider's boundary before it streams.
pub fn offer(config: &SourceConfig) -> Option<BootstrapCoordination> {
    if config.settings.start_from != StartFrom::Snapshot {
        return None;
    }
    protocol(config.mode).map(|protocol| BootstrapCoordination {
        protocol,
        required: true,
    })
}

/// Adds the configured source's coordination offer to its description.
pub(crate) fn declare(
    mode: Mode,
    request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    description: drasi_computation_plugin_sdk::ComponentDescription,
) -> Result<drasi_computation_plugin_sdk::ComponentDescription> {
    let config = SourceConfig::from_value(mode, &request.request.configuration)?;
    Ok(match offer(&config) {
        Some(offer) => description.with_coordination(offer),
        None => description,
    })
}

pub(crate) struct Coordinator {
    protocol: BootstrapProtocol,
    request: CreateRequest,
    config: SourceConfig,
    members: BTreeMap<ComponentId, SourceProgressView>,
    activation: ActivationCredentials,
}

impl Coordinator {
    /// `config` is the request's configuration, already parsed and validated.
    pub(crate) fn new(
        request: &CreateRequest,
        config: SourceConfig,
        progress: SourceBootstrapProgress,
    ) -> Result<Self> {
        let scope = request
            .scope
            .as_ref()
            .context("MySQL coordination requires graph scope")?;
        let protocol = offer(&config)
            .with_context(|| {
                format!(
                    "MySQL {:?} source coordinates bootstrap only with startFrom: snapshot",
                    config.mode
                )
            })?
            .protocol;
        let members = match (config.mode, progress) {
            (Mode::Transactions, SourceBootstrapProgress::Single(reader)) => {
                anyhow::ensure!(
                    reader.graph_id() == scope.graph_id && reader.component_id() != &request.id,
                    "MySQL coordinator progress belongs to another graph or the source"
                );
                BTreeMap::from([(reader.component_id().clone(), reader)])
            }
            (
                Mode::SharedTransactions | Mode::SharedChanges,
                SourceBootstrapProgress::Shared { consumers, members },
            ) => {
                anyhow::ensure!(
                    consumers.graph_id() == scope.graph_id
                        && consumers.producer_id() == &request.id,
                    "MySQL coordinator consumer set belongs to another graph or source"
                );
                members
            }
            _ => anyhow::bail!("MySQL coordinator progress mode does not match its factory"),
        };
        Ok(Self {
            protocol,
            request: request.clone(),
            config,
            members,
            activation: Arc::new(RwLock::new(None)),
        })
    }

    fn validate_request(&self, request: &CreateRequest) -> Result<()> {
        let (expected, actual) = (
            self.request
                .scope
                .as_ref()
                .context("MySQL coordinator scope is missing")?,
            request
                .scope
                .as_ref()
                .context("MySQL source scope is missing")?,
        );
        anyhow::ensure!(
            request.id == self.request.id
                && request.implementation == self.request.implementation
                && request.configuration_version == self.request.configuration_version
                && request.configuration == self.request.configuration
                && actual.instance_id == expected.instance_id
                && actual.graph_id == expected.graph_id,
            "MySQL source does not match its actual coordinator binding"
        );
        Ok(())
    }
}

#[async_trait]
impl NativeSourceBootstrapCoordinator for Coordinator {
    fn protocol(&self) -> BootstrapProtocol {
        self.protocol.clone()
    }

    fn create_source(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        live: SourceBootstrapLiveProgress,
    ) -> Result<CreatedComponent> {
        self.validate_request(request)?;
        let id = request.id.clone();
        let stream = StreamId::try_new(self.config.stream.clone())?;
        let engine = self.config.engine_config(&id)?;
        let consumers = |reader| {
            let members: Vec<_> = self.members.keys().cloned().collect();
            ConsumerBinding::new(&id, reader, &members)
        };
        let source = match (self.config.mode, live) {
            (Mode::Transactions, SourceBootstrapLiveProgress::Single(reader)) => {
                anyhow::ensure!(
                    self.members.contains_key(reader.component_id()),
                    "MySQL live progress does not match the coordinator owner"
                );
                MySqlSource::with_published_bootstrap(
                    id.clone(),
                    stream,
                    engine,
                    Some(reader),
                    None,
                )?
            }
            (Mode::SharedTransactions, SourceBootstrapLiveProgress::Shared(reader)) => {
                MySqlSource::with_published_bootstrap(
                    id.clone(),
                    stream,
                    engine,
                    None,
                    Some(consumers(reader)?),
                )?
            }
            (Mode::SharedChanges, SourceBootstrapLiveProgress::Shared(reader)) => {
                MySqlSource::shared_rows(
                    id.clone(),
                    stream,
                    engine,
                    consumers(reader)?,
                )?
            }
            _ => anyhow::bail!("MySQL source/bootstrap live progress mode changed"),
        };
        Ok(Component::Source(Box::new(MySqlComponent {
            config: self.config.clone(),
            source: source.with_status(status_sink(&id)),
            activation: Some(self.activation.clone()),
        }))
        .into())
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
            .context("query is not a MySQL coordinator consumer")?;
        anyhow::ensure!(
            expected.same_owner(&progress),
            "MySQL bootstrap requires its captured actual query reader"
        );
        let source = self.request.id.clone();
        let stream = StreamId::try_new(self.config.stream.clone())?;
        let engine = self.config.engine_config(&source)?;
        let inner = match self.config.mode {
            Mode::Transactions => MySqlSnapshot::new(source, stream, engine, progress)?,
            Mode::SharedTransactions => {
                MySqlSnapshot::for_consumer(source, stream, engine, progress)?
            }
            Mode::SharedChanges => MySqlSnapshot::shared_rows(source, stream, engine, progress)?,
            mode => anyhow::bail!("MySQL {mode:?} source does not pair with a bootstrap"),
        };
        Ok(Arc::new(Provider {
            instance: self
                .request
                .scope
                .as_ref()
                .context("MySQL coordinator scope is missing")?
                .instance_id
                .clone(),
            inner: inner
                .with_labels(labels.cloned())
                .with_activation(self.activation.clone()),
        }))
    }

    /// Providers own their snapshot workers and are stopped individually.
    async fn stop(&self) -> Result<()> {
        Ok(())
    }
}

/// A query's coordinated MySQL snapshot.
struct Provider {
    instance: String,
    inner: MySqlSnapshot,
}

#[async_trait]
impl DurableBootstrapProvider for Provider {
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
    async fn prepare(&self, state: Option<&dyn BootstrapState>) -> Result<BootstrapPreparation> {
        let state = state.context("MySQL bootstrap requires query-owned handover state")?;
        self.inner.prepare_with_state(state).await
    }
    fn has_pending_snapshot(&self) -> Result<bool> {
        Ok(false)
    }
    async fn snapshot(
        &self,
        state: Option<&dyn BootstrapState>,
    ) -> Result<ComputationBootstrapSnapshot> {
        let state = state.context("MySQL bootstrap requires query-owned handover state")?;
        self.inner.snapshot_with_state(state).await
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        self.inner.complete_snapshot().await
    }
    fn completion_state(&self) -> Result<Option<bytes::Bytes>> {
        self.inner.completion_state()
    }
    async fn stop(&self) -> Result<()> {
        self.inner.stop().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(mode: Mode, start: &str) -> Result<SourceConfig> {
        let mut value = serde_json::json!({
            "stream": "items/out", "database": "app", "user": "drasi", "startFrom": start,
            "tables": ["items"]
        });
        if mode == Mode::SharedChanges {
            value["persistent"] = serde_json::json!(true);
        }
        SourceConfig::from_value(mode, &value)
    }

    #[test]
    fn snapshot_start_offers_and_requires_the_modes_protocol() -> Result<()> {
        for mode in [
            Mode::Transactions,
            Mode::SharedTransactions,
            Mode::SharedChanges,
        ] {
            let offered = offer(&config(mode, "snapshot")?).context("offered")?;
            assert!(offered.required);
            assert_eq!(Some(offered.protocol), protocol(mode));
        }
        assert_eq!(offer(&config(Mode::SharedChanges, "end")?), None);
        assert_eq!(
            protocol(Mode::Changes),
            None,
            "untracked sources do not pair"
        );
        Ok(())
    }
}
