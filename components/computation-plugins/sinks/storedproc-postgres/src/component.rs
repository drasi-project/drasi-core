// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The external consumer: the host `DeliveryRunner` owns local progress and
//! retention; PostgreSQL commits each operation's procedure call together with
//! its destination cursor row.

use crate::{
    config::{Command, Commands, PostgresStoredProcConfig},
    connection::{bounded, ConnectError, Connection, Driver, Login},
    management,
    retry::Backoff,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, DeliveryHandler, DeliveryItem, SinkOutputs,
};
use drasi_computation_plugin_sdk::lifecycle::{
    nothing_to_do, ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext,
    LifecycleError, LifecycleResult, PauseContext, ResumeContext, StartContext, StopContext,
    StopMode, UpdateBoundary,
};
use drasi_computation_plugin_sdk::{
    ActivationCredentials, ApplyUpdateRequest, NativeComponent, NativeManagement, UpdateRequest,
};
use drasi_postgres_common::{
    delivery::progress::{DeliveryError, DeliveryOwner, ProgressEffect, ProgressStore},
    sql::{build_native_render_input, execute_parameters},
};
use drasi_reaction_common::OperationType;
use std::fmt;
use tokio_postgres::Transaction;

/// A command template that failed to render: deterministic and permanent.
#[derive(Debug)]
pub struct RenderError {
    alias: String,
    operation: OperationType,
    source: anyhow::Error,
}

impl RenderError {
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            alias: "people".into(),
            operation: OperationType::Add,
            source: anyhow::anyhow!("missing field"),
        }
    }
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the {:?} command template for output '{}' failed to render: {:#}",
            self.operation, self.alias, self.source
        )
    }
}

impl std::error::Error for RenderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

struct ProcedureEffect<'a> {
    outputs: &'a SinkOutputs,
    commands: &'a Commands,
}

#[async_trait]
impl ProgressEffect for ProcedureEffect<'_> {
    async fn apply(&self, transaction: &Transaction<'_>, item: &DeliveryItem<'_>) -> Result<()> {
        let output = self.outputs.output_for(item.envelope.system().stream())?;
        let input = build_native_render_input(item.envelope, item.operation, output)?;
        let Command::Template(template) = self.commands.resolve(&input.alias, input.operation)
        else {
            log::debug!(
                "PostgreSQL sink: no {:?} command configured for output '{}'",
                input.operation,
                input.alias
            );
            return Ok(());
        };
        let (command, parameters) =
            template
                .render(&input.context)
                .map_err(|source| RenderError {
                    alias: input.alias.clone(),
                    operation: input.operation,
                    source,
                })?;
        if command.trim().is_empty() {
            log::debug!(
                "PostgreSQL sink: the command condition excluded output '{}'",
                input.alias
            );
            return Ok(());
        }
        execute_parameters(transaction, &command, parameters).await?;
        Ok(())
    }
}

/// Whether a delivery failure may succeed on another attempt.
pub(crate) fn retryable(error: &anyhow::Error) -> bool {
    if let Some(error) = error.downcast_ref::<DeliveryError>() {
        return error.retryable();
    }
    if let Some(error) = error.downcast_ref::<ConnectError>() {
        return error.retryable();
    }
    false
}

/// The native PostgreSQL stored-procedure consumer.
pub struct PostgresStoredProcSink {
    descriptor: ComponentDescriptor,
    config: PostgresStoredProcConfig,
    outputs: SinkOutputs,
    commands: Commands,
    store: ProgressStore,
    /// The activation's connection slot; `Some` while started.
    active: Option<Option<Connection>>,
    /// A driver whose graceful close did not finish within a Stop deadline.
    retiring: Option<Driver>,
    /// The last activation's login, kept in memory only, for reconnects and
    /// Erase. Never persisted or exposed through `configuration()`.
    login: Option<Login>,
    paused: bool,
}

impl PostgresStoredProcSink {
    pub fn new(
        descriptor: ComponentDescriptor,
        config: PostgresStoredProcConfig,
        outputs: SinkOutputs,
        owner: DeliveryOwner,
    ) -> Result<Self> {
        config.validate()?;
        config.validate_outputs(&outputs)?;
        let commands = Commands::compile(&config)?;
        let store = ProgressStore::new(config.table()?, owner, config.max_streams)?;
        Ok(Self {
            descriptor,
            config,
            outputs,
            commands,
            store,
            active: None,
            retiring: None,
            login: None,
            paused: false,
        })
    }

    /// The destination cursor owner of this consumer.
    pub fn owner(&self) -> &DeliveryOwner {
        self.store.owner()
    }

    fn configured_login(&self) -> Login {
        Login {
            user: self.config.user.clone(),
            password: self.config.password.clone(),
        }
    }

    fn login(&self) -> Login {
        self.login
            .clone()
            .unwrap_or_else(|| self.configured_login())
    }

    /// Joins a driver left by a Stop that ran out of time.
    async fn join_retiring(&mut self, deadline: Option<tokio::time::Instant>) -> LifecycleResult {
        let Some(driver) = self.retiring.as_mut() else {
            return Ok(());
        };
        let limit = bounded(self.config.shutdown_timeout(), deadline);
        match tokio::time::timeout(limit, driver).await {
            Ok(result) => {
                self.retiring = None;
                match result {
                    Ok(Ok(())) => {}
                    // An earlier connection failure is not a Stop failure.
                    Ok(Err(error)) => log::debug!("PostgreSQL sink connection closed: {error}"),
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => log::warn!("PostgreSQL sink connection driver failed: {error}"),
                }
                Ok(())
            }
            Err(_) => Err(LifecycleError::retryable(anyhow::anyhow!(
                "the PostgreSQL connection did not close within {limit:?}; its driver is retained \
                 and Stop may be retried"
            ))),
        }
    }

    /// Connects, prepares the progress table and the cursor statements.
    async fn activate(
        &self,
        login: &Login,
        deadline: Option<tokio::time::Instant>,
    ) -> LifecycleResult<Connection> {
        let mut connection = Connection::connect(&self.config, login, deadline)
            .await
            .map_err(|error| {
                if error.retryable() {
                    LifecycleError::retryable(error)
                } else {
                    LifecycleError::terminal(error)
                }
            })?;
        let limit = bounded(self.config.client_command_timeout(), deadline);
        if let Err(error) = self
            .store
            .ensure_table(&mut connection.client, Some(limit))
            .await
        {
            let retry = error.retryable();
            connection.discard().await;
            return Err(if retry {
                LifecycleError::retryable(error)
            } else {
                LifecycleError::terminal(error)
            });
        }
        if let Err(error) = connection.ready(&self.store, &self.config, limit).await {
            let retry = error.retryable();
            connection.discard().await;
            return Err(if retry {
                LifecycleError::retryable(error)
            } else {
                LifecycleError::terminal(error)
            });
        }
        Ok(connection)
    }

    async fn erase(&self, ctx: &DisposeContext) -> Result<u64> {
        let deadline = ctx.deadline();
        let connection = Connection::connect(&self.config, &self.login(), deadline).await?;
        let limit = bounded(self.config.client_command_timeout(), deadline);
        let result = self.store.erase(&connection.client, Some(limit)).await;
        if let Some(driver) = connection.close() {
            let limit = bounded(self.config.shutdown_timeout(), deadline);
            if tokio::time::timeout(limit, driver).await.is_err() {
                log::warn!("PostgreSQL sink erase connection did not close within {limit:?}");
            }
        }
        Ok(result?)
    }

    async fn release(&mut self) {
        if let Some(Some(connection)) = self.active.take() {
            connection.discard().await;
        }
        if let Some(driver) = self.retiring.take() {
            driver.abort();
            let _ = driver.await;
        }
        self.paused = false;
    }
}

fn credential_login(credentials: ActivationCredentials) -> Result<Login> {
    credentials.validate()?;
    let (user, password) = credentials.into_auth_pair();
    anyhow::ensure!(
        !user.is_empty() && !user.contains('\0') && !password.contains('\0'),
        "activation credentials need a nonempty user without NUL"
    );
    Ok(Login { user, password })
}

#[async_trait]
impl NativeComponent for PostgresStoredProcSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    /// The complete configuration, including secrets: the host persists it
    /// through secret references. Activation credentials are never included.
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.config)?)
    }

    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }

    /// Templates, TLS material and routes were validated at construction;
    /// nothing is opened here.
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        nothing_to_do()
    }

    /// Connects, prepares the progress table and the cursor statements within
    /// `connectTimeoutMs`, `commandTimeoutMs` and the lifecycle deadline. A
    /// cancelled or failed start leaves nothing running and the sink startable.
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        if ctx.cancellation().is_cancelled() {
            return Err(LifecycleError::cancelled());
        }
        if self.active.is_some() {
            return Err(LifecycleError::terminal(anyhow::anyhow!(
                "PostgreSQL sink is already started"
            )));
        }
        if let Some(driver) = &self.retiring {
            if !driver.is_finished() {
                return Err(LifecycleError::retryable(anyhow::anyhow!(
                    "PostgreSQL sink requires its previous connection to close before restart; \
                     retry Stop first"
                )));
            }
            self.join_retiring(ctx.deadline()).await?;
        }
        let login = match ctx.take_credentials() {
            Some(credentials) => credential_login(credentials).map_err(LifecycleError::terminal)?,
            None => self.configured_login(),
        };
        let deadline = ctx.deadline();
        let cancellation = ctx.cancellation().clone();
        let connection = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(LifecycleError::cancelled()),
            connection = self.activate(&login, deadline) => connection?,
        };
        self.login = Some(login);
        self.active = Some(Some(connection));
        self.paused = false;
        Ok(())
    }

    /// Host-driven (D-6): the runner calls no `handle` while paused and has
    /// already awaited or dropped the in-flight one. The connection driver
    /// only answers this sink's own commands and produces no work.
    async fn pause(&mut self, _ctx: &PauseContext) -> LifecycleResult {
        self.paused = true;
        Ok(())
    }

    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        self.paused = false;
        Ok(())
    }

    /// `Drain` closes the client and waits for the server to answer a command
    /// still running (bounded by `statement_timeout`); `Abort` drops the
    /// socket at once. On expiry the driver is retained and a retryable error
    /// is returned.
    async fn stop(&mut self, ctx: &StopContext) -> LifecycleResult {
        self.paused = false;
        if let Some(Some(connection)) = self.active.take() {
            if let Some(driver) = connection.close() {
                if ctx.mode() == StopMode::Abort {
                    driver.abort();
                }
                self.retiring = Some(driver);
            }
        }
        let cancellation = ctx.cancellation().clone();
        let deadline = ctx.deadline();
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(LifecycleError::cancelled()),
            result = self.join_retiring(deadline) => result,
        }
    }

    /// `Erase` first deletes this consumer's cursor rows (only
    /// `WHERE owner = <owner>`, idempotently), then releases everything.
    async fn dispose(&mut self, ctx: &DisposeContext) -> LifecycleResult {
        if ctx.erases_owned_state() {
            let cancellation = ctx.cancellation().clone();
            let erased = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(LifecycleError::cancelled()),
                erased = self.erase(ctx) => erased,
            };
            match erased {
                Ok(rows) => log::info!(
                    "PostgreSQL sink erased {rows} cursor rows of owner {} from {}",
                    self.store.owner(),
                    self.store.table()
                ),
                Err(error) => {
                    return Err(LifecycleError::retryable(
                        error.context("erasing the PostgreSQL sink's cursor rows"),
                    ))
                }
            }
        }
        self.release().await;
        self.login = None;
        Ok(())
    }
}

fn refused(reason: impl fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
    }
}

#[async_trait]
impl NativeManagement for PostgresStoredProcSink {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        management::assess(request)
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        let current = PostgresStoredProcConfig::parse(update.request.current.configuration.clone())
            .map_err(refused)?;
        let proposed =
            PostgresStoredProcConfig::parse(update.request.proposed.configuration.clone())
                .map_err(refused)?;
        if current != self.config {
            return Err(refused(
                "the sink configuration changed after the update was assessed",
            ));
        }
        let assessment = management::assess(&update.request).map_err(refused)?;
        let Some(plan) = assessment.plan() else {
            return Err(refused("the update is not supported"));
        };
        if plan != update.plan {
            return Err(ApplyError::Refused {
                now_requires: Some(plan.mode.into()),
                reason: "the update now requires a different boundary".into(),
            });
        }
        let required = UpdateBoundary::from(plan.mode);
        let boundary = ctx.boundary();
        if boundary < required {
            return Err(ApplyError::Refused {
                now_requires: Some(plan.mode.into()),
                reason: format!("the update requires the {required:?} boundary"),
            });
        }
        match boundary {
            UpdateBoundary::Pause if self.active.is_some() && !self.paused => {
                return Err(refused("in-place updates require a completed pause"));
            }
            UpdateBoundary::Stop if self.active.is_some() => {
                return Err(refused("restart updates require a stopped sink"));
            }
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
            UpdateBoundary::Pause | UpdateBoundary::Stop => {}
        }
        let commands = Commands::compile(&proposed).map_err(refused)?;
        let store = ProgressStore::new(
            proposed.table().map_err(refused)?,
            self.store.owner().clone(),
            proposed.max_streams,
        )
        .map_err(refused)?;
        self.commands = commands;
        self.store = store;
        self.config = proposed;
        Ok(())
    }
}

#[async_trait]
impl DeliveryHandler for PostgresStoredProcSink {
    fn retryable(&self, error: &anyhow::Error) -> bool {
        retryable(error)
    }

    fn delivery_guarantee(&self) -> drasi_computation_contracts::DeliveryGuarantee {
        self.config.delivery_guarantee
    }

    /// Delivers one operation. Transient failures (rolled back, connection
    /// lost, commit outcome unknown) are retried within `reconnectBudgetMs`,
    /// reconnecting as needed; the destination cursor makes every retry
    /// idempotent. A failure never prevents a later `handle`.
    async fn handle(&mut self, item: DeliveryItem<'_>) -> Result<()> {
        anyhow::ensure!(
            item.port.as_str() == "in",
            "unknown PostgreSQL sink input port"
        );
        if item.position.epoch().is_some() && !self.config.delivery_guarantee.is_at_least_once() {
            // Internal invariant guard only: the graph refuses this pairing when
            // the edge is configured.
            return Err(
                drasi_computation_contracts::Incompatibility::ProducerIdentity {
                    required: drasi_computation_contracts::DeliveryGuarantee::ExactlyOnce,
                    identity: drasi_computation_contracts::OutputIdentity::Volatile,
                }
                .into(),
            );
        }
        let login = self.login();
        let Self {
            config,
            outputs,
            commands,
            store,
            active,
            ..
        } = self;
        let slot = active.as_mut().context("PostgreSQL sink is not running")?;
        let effect = ProcedureEffect { outputs, commands };
        let mut backoff = Backoff::new(config.reconnect_budget());
        loop {
            let error = match attempt(config, store, slot, &login, &effect, &item).await {
                Ok(()) => return Ok(()),
                Err(error) => error,
            };
            if !retryable(&error) || !backoff.wait().await {
                return Err(error);
            }
            log::warn!("Retrying PostgreSQL delivery {}: {error:#}", item.id);
        }
    }
}

async fn attempt(
    config: &PostgresStoredProcConfig,
    store: &ProgressStore,
    slot: &mut Option<Connection>,
    login: &Login,
    effect: &ProcedureEffect<'_>,
    item: &DeliveryItem<'_>,
) -> Result<()> {
    if slot.as_ref().is_some_and(|connection| !connection.usable()) {
        if let Some(connection) = slot.take() {
            log::debug!("PostgreSQL sink replaces an unusable connection");
            connection.discard().await;
        }
    }
    if slot.is_none() {
        *slot = Some(Connection::connect(config, login, None).await?);
    }
    let connection = slot.as_mut().context("PostgreSQL connection missing")?;
    let limit = config.client_command_timeout();
    connection.in_flight = true;
    let result = match connection.ready(store, config, limit).await {
        Ok(()) => match &connection.prepared {
            Some(prepared) => {
                store
                    .deliver(&mut connection.client, prepared, effect, item, Some(limit))
                    .await
            }
            None => Err(DeliveryError::Rejected(
                "cursor statements are not prepared".into(),
            )),
        },
        Err(error) => Err(error),
    };
    connection.in_flight = false;
    if let Err(error) = &result {
        if error.discards_connection() {
            if let Some(connection) = slot.take() {
                connection.discard().await;
            }
        }
    }
    Ok(result?)
}

impl Drop for PostgresStoredProcSink {
    fn drop(&mut self) {
        if self.retiring.is_some() || matches!(self.active, Some(Some(_))) {
            log::warn!("Native PostgreSQL sink dropped without awaited connection cleanup");
        }
        if let Some(driver) = self.retiring.take() {
            driver.abort();
        }
    }
}
