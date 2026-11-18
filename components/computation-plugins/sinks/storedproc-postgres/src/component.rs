// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The external consumer: the host `DeliveryRunner` owns local progress and
//! retention; PostgreSQL commits each operation's procedure call together with
//! its destination cursor row.

use crate::{
    config::{Command, Commands, PostgresStoredProcConfig},
    connection::{bounded, Login, PgConnector},
    management,
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
use drasi_computation_storedproc_common::{
    driver::{Delivery, ErrorClass, Session},
    OperationContext, Position,
};
use drasi_computation_template_common::{BoundCommand, TemplateError};
use drasi_postgres_common::delivery::progress::PgSession;
use drasi_reaction_common::OperationType;
use std::fmt;

/// A command template that failed to render: deterministic and permanent.
#[derive(Debug)]
pub struct RenderError {
    alias: String,
    operation: OperationType,
    source: TemplateError,
}

impl RenderError {
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            alias: "people".into(),
            operation: OperationType::Add,
            source: TemplateError::Render("missing field".into()),
        }
    }
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the {:?} command template for output '{}' failed to render: {}",
            self.operation, self.alias, self.source
        )
    }
}

impl std::error::Error for RenderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Renders the command of one operation: `None` when no template applies
/// or its condition rendered nothing.
fn render(
    outputs: &SinkOutputs,
    commands: &Commands,
    item: &DeliveryItem<'_>,
) -> Result<Option<(String, Vec<serde_json::Value>)>> {
    let input = OperationContext::new(item, outputs)?;
    let Command::Template(template) = commands.resolve(&input.alias, input.operation) else {
        log::debug!(
            "PostgreSQL sink: no {:?} command configured for output '{}'",
            input.operation,
            input.alias
        );
        return Ok(None);
    };
    let BoundCommand {
        sql: command,
        parameters,
    } = template
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
        return Ok(None);
    }
    Ok(Some((command, parameters)))
}

/// A lifecycle error for a failed start or erase step.
fn lifecycle(error: anyhow::Error) -> LifecycleError {
    match PgSession::classify(&error) {
        ErrorClass::Transient { .. } => LifecycleError::retryable(error),
        ErrorClass::Permanent => LifecycleError::terminal(error),
    }
}

/// The native PostgreSQL stored-procedure consumer.
pub struct PostgresStoredProcSink {
    descriptor: ComponentDescriptor,
    config: PostgresStoredProcConfig,
    outputs: SinkOutputs,
    commands: Commands,
    owner: String,
    /// The activation's delivery engine; `Some` while started.
    delivery: Option<Delivery<PgConnector>>,
    /// The last activation's login, kept in memory only, for Erase after
    /// Stop. Never persisted or exposed through `configuration()`.
    login: Option<Login>,
    paused: bool,
}

impl PostgresStoredProcSink {
    pub fn new(
        descriptor: ComponentDescriptor,
        config: PostgresStoredProcConfig,
        outputs: SinkOutputs,
        owner: String,
    ) -> Result<Self> {
        config.validate()?;
        config.validate_outputs(&outputs)?;
        let commands = Commands::compile(&config)?;
        Ok(Self {
            descriptor,
            config,
            outputs,
            commands,
            owner,
            delivery: None,
            login: None,
            paused: false,
        })
    }

    /// The destination cursor owner of this consumer.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    fn configured_login(&self) -> Login {
        Login {
            user: self.config.user.clone(),
            password: self.config.password.clone(),
        }
    }

    fn delivery(&self, login: Login) -> Delivery<PgConnector> {
        Delivery::new(
            PgConnector::new(self.config.clone(), login, self.owner.clone()),
            self.config.max_streams,
            self.config.on_permanent_failure,
        )
    }

    async fn erase(&self, ctx: &DisposeContext) -> Result<u64> {
        let login = self
            .login
            .clone()
            .unwrap_or_else(|| self.configured_login());
        let mut delivery = self.delivery(login);
        let erased = delivery.erase().await;
        let limit = bounded(self.config.shutdown_timeout(), ctx.deadline());
        if let Err(error) = delivery.close(limit).await {
            log::warn!("PostgreSQL sink erase connection close: {error:#}");
        }
        erased
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

    /// Connects and prepares the progress and dead-letter tables and the
    /// cursor statements: one attempt within `connectTimeoutMs`,
    /// `commandTimeoutMs` and the lifecycle deadline. A transient failure is
    /// retryable (the host's lifecycle retry policy decides); a cancelled or
    /// failed start leaves nothing open and the sink startable.
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        if ctx.cancellation().is_cancelled() {
            return Err(LifecycleError::cancelled());
        }
        if self.delivery.is_some() {
            return Err(LifecycleError::terminal(anyhow::anyhow!(
                "PostgreSQL sink is already started"
            )));
        }
        let login = match ctx.take_credentials() {
            Some(credentials) => credential_login(credentials).map_err(LifecycleError::terminal)?,
            None => self.configured_login(),
        };
        let mut delivery = self.delivery(login.clone());
        let cancellation = ctx.cancellation().clone();
        let deadline = async {
            match ctx.deadline() {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(LifecycleError::cancelled()),
            _ = deadline => return Err(LifecycleError::retryable(anyhow::anyhow!(
                "PostgreSQL sink start deadline expired"
            ))),
            prepared = delivery.prepare() => prepared.map_err(lifecycle)?,
        }
        self.login = Some(login);
        self.delivery = Some(delivery);
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

    /// `Drain` closes the idle connection gracefully within
    /// `shutdownTimeoutMs` and the deadline; `Abort` (or an expired close)
    /// drops it, and the server rolls back anything open. Nothing outlives
    /// Stop.
    async fn stop(&mut self, ctx: &StopContext) -> LifecycleResult {
        self.paused = false;
        let Some(mut delivery) = self.delivery.take() else {
            return Ok(());
        };
        if ctx.mode() == StopMode::Drain && !ctx.cancellation().is_cancelled() {
            let limit = bounded(self.config.shutdown_timeout(), ctx.deadline());
            if let Err(error) = delivery.close(limit).await {
                log::debug!("PostgreSQL sink connection close: {error:#}");
            }
        }
        Ok(())
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
                    self.owner,
                    self.config.progress_table
                ),
                Err(error) => {
                    return Err(LifecycleError::retryable(
                        error.context("erasing the PostgreSQL sink's cursor rows"),
                    ))
                }
            }
        }
        self.delivery = None;
        self.paused = false;
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
            UpdateBoundary::Pause if self.delivery.is_some() && !self.paused => {
                return Err(refused("in-place updates require a completed pause"));
            }
            UpdateBoundary::Stop if self.delivery.is_some() => {
                return Err(refused("restart updates require a stopped sink"));
            }
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
            UpdateBoundary::Pause | UpdateBoundary::Stop => {}
        }
        let commands = Commands::compile(&proposed).map_err(refused)?;
        if let Some(delivery) = self.delivery.as_mut() {
            delivery.set_max_streams(proposed.max_streams);
            if proposed.command_timeout_ms != self.config.command_timeout_ms {
                // The next operation reconnects with the new statement bound.
                let limit = self.config.shutdown_timeout();
                if let Err(error) = delivery.close(limit).await {
                    log::debug!("PostgreSQL sink connection close: {error:#}");
                }
            }
            delivery.connector_mut().set_config(proposed.clone());
        }
        self.commands = commands;
        self.config = proposed;
        Ok(())
    }
}

#[async_trait]
impl DeliveryHandler for PostgresStoredProcSink {
    fn delivery_guarantee(&self) -> drasi_computation_contracts::DeliveryGuarantee {
        self.config.delivery_guarantee
    }

    /// Delivers one operation in one transaction with its cursor row (see
    /// `drasi_computation_storedproc_common::driver`): one attempt, classified
    /// for the graph, which retries transient failures; under
    /// `onPermanentFailure: skip` a content failure is settled as skipped.
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
        let position = Position::from_item(&item)?;
        let Self {
            outputs,
            commands,
            delivery,
            ..
        } = self;
        let delivery = delivery
            .as_mut()
            .context("PostgreSQL sink is not running")?;
        let mut render = || render(outputs, commands, &item);
        delivery.deliver(&position, &mut render).await.map(drop)
    }
}

impl Drop for PostgresStoredProcSink {
    fn drop(&mut self) {
        if self.delivery.is_some() {
            log::warn!("Native PostgreSQL sink dropped without a Stop");
        }
    }
}
