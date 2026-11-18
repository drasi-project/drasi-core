// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Pairs a PostgreSQL source with its coordinated or shared bootstrap
//! providers. The coordinator constructs the source and opens each query's
//! provider from the same request, so both use one identity, stream,
//! publication, table selection and managed-slot prefix; nothing is copied
//! into the provider's configuration.

use super::{snapshot::Activation, PostgresSharedSnapshot, PostgresSnapshot};
use crate::{
    component::PostgresTransactionSource,
    config::{Mode, SourceConfig},
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    BootstrapLabels, BootstrapProtocol, ComponentId, SourceProgressView, StreamId,
};
use drasi_computation_plugin_sdk::{
    Component, ControlSender, CreateRequest, CreatedComponent, DurableBootstrapProvider,
    NativeSourceBootstrapCoordinator, SourceBootstrapLiveProgress, SourceBootstrapProgress,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub(crate) struct Coordinator {
    request: CreateRequest,
    mode: Mode,
    config: SourceConfig,
    members: BTreeMap<ComponentId, SourceProgressView>,
    /// The constructed source's activation settings, once constructed.
    activation: Mutex<Option<Activation>>,
}

impl Coordinator {
    /// `config` is the request's configuration, already parsed and validated
    /// by the factory for `mode`.
    pub(crate) fn new(
        request: &CreateRequest,
        mode: Mode,
        config: SourceConfig,
        progress: SourceBootstrapProgress,
    ) -> Result<Self> {
        let scope = request
            .scope
            .as_ref()
            .context("PostgreSQL coordination requires graph scope")?;
        anyhow::ensure!(
            super::offer(mode, config.coordinated_snapshot).is_some(),
            "PostgreSQL {mode:?} source does not coordinate bootstrap without coordinatedSnapshot"
        );
        let members = match (mode, progress) {
            (Mode::Transactions | Mode::Rows, SourceBootstrapProgress::Single(reader)) => {
                anyhow::ensure!(
                    reader.graph_id() == scope.graph_id && reader.component_id() != &request.id,
                    "PostgreSQL coordinator progress belongs to another graph or the source"
                );
                BTreeMap::from([(reader.component_id().clone(), reader)])
            }
            (
                Mode::SharedRows | Mode::SharedTransactions,
                SourceBootstrapProgress::Shared { consumers, members },
            ) => {
                anyhow::ensure!(
                    consumers.graph_id() == scope.graph_id
                        && consumers.producer_id() == &request.id,
                    "PostgreSQL coordinator consumer set belongs to another graph or source"
                );
                members
            }
            _ => anyhow::bail!("PostgreSQL coordinator progress mode does not match its factory"),
        };
        Ok(Self {
            request: request.clone(),
            mode,
            config,
            members,
            activation: Mutex::new(None),
        })
    }

    fn validate_request(&self, request: &CreateRequest) -> Result<()> {
        let (expected, actual) = (
            self.request
                .scope
                .as_ref()
                .context("PostgreSQL coordinator scope is missing")?,
            request
                .scope
                .as_ref()
                .context("PostgreSQL source scope is missing")?,
        );
        anyhow::ensure!(
            request.id == self.request.id
                && request.implementation == self.request.implementation
                && request.configuration_version == self.request.configuration_version
                && request.configuration == self.request.configuration
                && actual.instance_id == expected.instance_id
                && actual.graph_id == expected.graph_id,
            "PostgreSQL source does not match its actual coordinator binding"
        );
        Ok(())
    }

    fn stream(&self) -> Result<StreamId> {
        Ok(StreamId::try_new(self.config.stream.clone())?)
    }
}

#[async_trait]
impl NativeSourceBootstrapCoordinator for Coordinator {
    fn protocol(&self) -> BootstrapProtocol {
        super::protocol(self.mode)
    }

    fn create_source(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        live: SourceBootstrapLiveProgress,
    ) -> Result<CreatedComponent> {
        self.validate_request(request)?;
        let id = request.id.clone();
        let settings = self.config.settings.clone();
        let source = match (self.mode, live) {
            (Mode::Transactions, SourceBootstrapLiveProgress::Single(reader)) => {
                anyhow::ensure!(
                    self.members.contains_key(reader.component_id()),
                    "PostgreSQL live progress does not match the coordinator owner"
                );
                PostgresTransactionSource::new(
                    id,
                    self.stream()?,
                    settings,
                    reader,
                    self.config.coordinated_snapshot,
                )?
            }
            (Mode::Rows, SourceBootstrapLiveProgress::Single(reader)) => {
                anyhow::ensure!(
                    self.members.contains_key(reader.component_id()),
                    "PostgreSQL live progress does not match the coordinator owner"
                );
                PostgresTransactionSource::rows(
                    id,
                    self.stream()?,
                    settings,
                    reader,
                    self.config.coordinated_snapshot,
                    self.config.persistent,
                )?
            }
            (Mode::SharedRows, SourceBootstrapLiveProgress::Shared(reader)) => {
                let consumers: Vec<_> = self.members.keys().cloned().collect();
                PostgresTransactionSource::shared_rows(
                    id,
                    self.stream()?,
                    settings,
                    reader,
                    &consumers,
                    self.config.persistent,
                )?
            }
            (Mode::SharedTransactions, SourceBootstrapLiveProgress::Shared(reader)) => {
                let consumers: Vec<_> = self.members.keys().cloned().collect();
                PostgresTransactionSource::shared_transactions_coordinated(
                    id,
                    self.stream()?,
                    settings,
                    reader,
                    &consumers,
                )?
            }
            _ => anyhow::bail!("PostgreSQL source/bootstrap live progress mode changed"),
        };
        *self
            .activation
            .lock()
            .map_err(|_| anyhow::anyhow!("PostgreSQL coordinator activation poisoned"))? =
            Some(source.activation_handle());
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
            .context("query is not a PostgreSQL coordinator consumer")?;
        anyhow::ensure!(
            expected.same_owner(&progress),
            "PostgreSQL bootstrap requires its captured actual query reader"
        );
        // The provider connects with the constructed source's activation
        // credentials (sessions are relayed only while the source is active),
        // or with the source's configured login before it is constructed.
        let activation = self
            .activation
            .lock()
            .map_err(|_| anyhow::anyhow!("PostgreSQL coordinator activation poisoned"))?
            .clone();
        let (source, stream, settings) = (
            self.request.id.clone(),
            self.stream()?,
            self.config.settings.clone(),
        );
        let labels = labels.cloned();
        Ok(match self.mode {
            Mode::Transactions => Arc::new(
                PostgresSnapshot::new(source, stream, settings, progress)?
                    .with_labels(labels)
                    .with_activation(activation.clone()),
            ),
            Mode::Rows => Arc::new(
                PostgresSnapshot::rows(source, stream, settings, progress)?
                    .with_labels(labels)
                    .with_activation(activation.clone()),
            ),
            Mode::SharedRows => Arc::new(
                PostgresSharedSnapshot::new(source, stream, settings, progress)?
                    .with_labels(labels)
                    .with_activation(activation.clone()),
            ),
            Mode::SharedTransactions => Arc::new(
                PostgresSharedSnapshot::transactions(source, stream, settings, progress)?
                    .with_labels(labels)
                    .with_activation(activation.clone()),
            ),
        })
    }

    /// Providers own their snapshot runs and are stopped individually; the
    /// coordinator holds no worker.
    async fn stop(&self) -> Result<()> {
        Ok(())
    }
}
