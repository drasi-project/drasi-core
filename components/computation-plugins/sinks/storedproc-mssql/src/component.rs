// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The native external consumer: the host `DeliveryRunner` owns input
//! progress and calls [`DeliveryHandler::handle`] once per operation; SQL
//! Server commits each operation's procedure effect with this sink's cursor.

use crate::{
    config::MsSqlStoredProcConfig,
    delivery::classify,
    management,
    session::{Authentication, TiberiusConnector},
};
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, DeliveryHandler, DeliveryItem, GraphChangeCodec,
    InputBinding, PipeRequirements, PortDescriptor, PortDirection, PortId, SinkOutputs,
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
    driver::Delivery, ErrorClass, OperationContext, Position,
};
use drasi_mssql_common::sql::{CommandTemplate, RenderedCommand};
use serde_json::Value;
use std::{collections::HashMap, future::Future, time::Duration};

/// Compiled templates keyed by their source text.
pub(crate) fn compile_templates(
    config: &MsSqlStoredProcConfig,
) -> Result<HashMap<String, CommandTemplate>> {
    let mut templates = HashMap::new();
    for (source, location) in config.templates() {
        if source.trim().is_empty() || templates.contains_key(source) {
            continue;
        }
        templates.insert(
            source.to_owned(),
            CommandTemplate::compile(source).with_context(|| format!("{location} template"))?,
        );
    }
    Ok(templates)
}

/// The SQL Server stored-procedure consumer.
pub struct MsSqlStoredProcSink {
    descriptor: ComponentDescriptor,
    config: MsSqlStoredProcConfig,
    outputs: SinkOutputs,
    templates: HashMap<String, CommandTemplate>,
    owner: String,
    identity_bound: bool,
    delivery: Option<Delivery<TiberiusConnector>>,
    /// The last activation's authentication, kept in memory only so that an
    /// `Erase` dispose after Stop can delete this sink's cursor rows.
    last_authentication: Option<Authentication>,
    paused: bool,
}

impl MsSqlStoredProcSink {
    pub(crate) fn ports() -> Result<Vec<PortDescriptor>> {
        Ok(vec![PortDescriptor::new(
            PortId::try_new("in")?,
            PortDirection::Input,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )])
    }

    pub fn describe(id: ComponentId) -> Result<ComponentDescriptor> {
        Ok(ComponentDescriptor::try_new(id, Self::ports()?)?)
    }

    /// Configuration-only construction: validates, compiles every template
    /// and resolves routes against the declared incoming edges (`inputs`).
    /// `graph_id` and the component ID determine the cursor owner.
    pub fn new(
        id: ComponentId,
        graph_id: &str,
        config: MsSqlStoredProcConfig,
        inputs: &[InputBinding],
        identity_bound: bool,
    ) -> Result<Self> {
        config.validate()?;
        config.validate_credentials(identity_bound)?;
        let outputs = SinkOutputs::from_inputs(inputs, &config.outputs)?;
        config.validate_outputs(&outputs)?;
        let templates = compile_templates(&config)?;
        let owner = drasi_computation_storedproc_common::owner(graph_id, id.as_str());
        Ok(Self {
            descriptor: Self::describe(id)?,
            config,
            outputs,
            templates,
            owner,
            identity_bound,
            delivery: None,
            last_authentication: None,
            paused: false,
        })
    }

    /// The cursor-table owner of this sink.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn config(&self) -> &MsSqlStoredProcConfig {
        &self.config
    }

    pub(crate) fn running(&self) -> bool {
        self.delivery.is_some()
    }

    /// The authentication for an activation: provider credentials when an
    /// identity is bound (never falling back to literals), else the literal
    /// SQL credentials.
    pub(crate) fn authentication(
        &self,
        credentials: Option<ActivationCredentials>,
    ) -> Result<Authentication> {
        match credentials {
            Some(credentials) => {
                ensure!(
                    self.identity_bound,
                    "SQL Server activation credentials were not bound by the host"
                );
                credentials.validate()?;
                match credentials {
                    ActivationCredentials::UsernamePassword { username, password } => {
                        ensure!(
                            !username.is_empty() && !password.is_empty(),
                            "the identity provider returned an empty SQL login or password"
                        );
                        Ok(Authentication::Sql {
                            user: username,
                            password,
                        })
                    }
                    // A token is a Microsoft Entra ID access token for
                    // `https://database.windows.net/`; the username is unused.
                    ActivationCredentials::Token { token, .. } => {
                        ensure!(
                            !token.is_empty(),
                            "the identity provider returned an empty access token"
                        );
                        Ok(Authentication::AccessToken { token })
                    }
                }
            }
            None => {
                ensure!(
                    !self.identity_bound,
                    "a provider-bound SQL Server sink cannot fall back to literal credentials"
                );
                Ok(Authentication::Sql {
                    user: self.config.user.clone(),
                    password: self.config.password.clone().unwrap_or_default(),
                })
            }
        }
    }

    /// Renders the command for one operation, or `None` when no template
    /// applies or the template renders nothing.
    pub(crate) fn render(
        config: &MsSqlStoredProcConfig,
        outputs: &SinkOutputs,
        templates: &mut HashMap<String, CommandTemplate>,
        item: &DeliveryItem<'_>,
    ) -> Result<Option<RenderedCommand>> {
        let input = OperationContext::new(item, outputs)?;
        let Some(source) = config.template_for(&input.alias, input.operation) else {
            return Ok(None);
        };
        let Some(template) = templates.get(source) else {
            // An empty template is not compiled: the operation is ignored.
            return Ok(None);
        };
        template.render(&input.context)
    }

    /// Renders one delivery item with this sink's templates.
    #[cfg(test)]
    pub(crate) fn render_item(
        &mut self,
        item: &DeliveryItem<'_>,
    ) -> Result<Option<RenderedCommand>> {
        Self::render(&self.config, &self.outputs, &mut self.templates, item)
    }

    fn delivery(&self, authentication: &Authentication) -> Result<Delivery<TiberiusConnector>> {
        Ok(Delivery::new(
            TiberiusConnector::new(&self.config, authentication, self.owner.clone())?,
            self.config.max_streams,
            self.config.on_permanent_failure,
        ))
    }

    fn warn_about_transport(&self) {
        self.config
            .connection_settings()
            .warn_about_transport("Native SQL Server sink");
    }

    async fn start_activation(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        if ctx.cancellation().is_cancelled() {
            return Err(LifecycleError::cancelled());
        }
        if self.delivery.is_some() {
            return Err(anyhow::anyhow!("SQL Server sink is already started").into());
        }
        let authentication = self.authentication(ctx.take_credentials())?;
        self.warn_about_transport();
        let mut delivery = self.delivery(&authentication)?;
        let prepared = bounded(ctx.cancellation(), ctx.deadline(), delivery.prepare()).await;
        match prepared {
            Ok(()) => {
                self.delivery = Some(delivery);
                self.last_authentication = Some(authentication);
                self.paused = false;
                Ok(())
            }
            Err(error) => Err(lifecycle_error(error)),
        }
    }

    async fn stop_activation(&mut self, ctx: &StopContext) -> LifecycleResult {
        self.paused = false;
        let Some(mut delivery) = self.delivery.take() else {
            return Ok(());
        };
        if ctx.mode() == StopMode::Drain && !ctx.cancellation().is_cancelled() {
            // The host never runs `handle` concurrently with Stop, so there is
            // no in-flight statement: Drain only closes the idle connection.
            let limit = self.config.shutdown_timeout();
            let timeout = remaining(ctx.deadline()).map_or(limit, |r| r.min(limit));
            if let Err(error) = delivery.close(timeout).await {
                log::debug!("SQL Server sink connection close: {error:#}");
            }
        }
        // Abort (or an expired close) drops the socket; SQL Server rolls back
        // anything open. Nothing outlives Stop, so ownership is never retained.
        Ok(())
    }

    async fn erase(&mut self, ctx: &DisposeContext) -> Result<()> {
        let authentication = match (&self.last_authentication, self.identity_bound) {
            (Some(authentication), _) => authentication.clone(),
            (None, false) => self.authentication(None)?,
            (None, true) => bail!(
                "erasing this sink's cursor rows needs the activation identity, which is \
                 only available after a start in this process; delete them manually with \
                 DELETE FROM {} WHERE owner = N'{}'",
                crate::session::quoted(&self.config.progress_table()?),
                self.owner
            ),
        };
        let mut delivery = self.delivery(&authentication)?;
        let erased = bounded(ctx.cancellation(), ctx.deadline(), delivery.erase()).await?;
        log::info!(
            "SQL Server sink erased {erased} cursor row(s) for owner {}",
            self.owner
        );
        let _ = delivery.close(self.config.shutdown_timeout()).await;
        Ok(())
    }
}

fn remaining(deadline: Option<tokio::time::Instant>) -> Option<Duration> {
    deadline.map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()))
}

/// Runs `work` until it finishes, the token is cancelled or the deadline
/// passes. Dropping `work` drops any connection it owns.
async fn bounded<T>(
    cancellation: &drasi_computation_plugin_sdk::lifecycle::CancellationToken,
    deadline: Option<tokio::time::Instant>,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    let deadline = async {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(anyhow::Error::new(Cancelled)),
        _ = deadline => Err(anyhow::anyhow!("SQL Server sink lifecycle deadline expired")),
        result = work => result,
    }
}

#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the lifecycle operation was cancelled")
    }
}

impl std::error::Error for Cancelled {}

fn lifecycle_error(error: anyhow::Error) -> LifecycleError {
    if error.downcast_ref::<Cancelled>().is_some() {
        LifecycleError::cancelled()
    } else if matches!(classify(&error), ErrorClass::Transient { .. }) {
        LifecycleError::retryable(error)
    } else {
        error.into()
    }
}

#[async_trait]
impl NativeComponent for MsSqlStoredProcSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<Value> {
        Ok(serde_json::to_value(&self.config)?)
    }
    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }
    /// Templates were compiled and routes resolved at construction; nothing
    /// connects here.
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        nothing_to_do()
    }
    /// Connects, creates or checks the progress table and keeps the
    /// connection, within the start deadline, the cancellation token and
    /// `retryBudgetMs`. A failed or cancelled start leaves nothing open.
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        self.start_activation(ctx).await
    }
    /// Host-driven (D-6): the runner calls no `handle` while paused and
    /// settles the in-flight one first. The idle connection stays open.
    async fn pause(&mut self, _ctx: &PauseContext) -> LifecycleResult {
        self.paused = true;
        Ok(())
    }
    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        self.paused = false;
        Ok(())
    }
    /// Drain closes the connection gracefully within the deadline; Abort
    /// drops it. An earlier delivery failure is never reported here.
    async fn stop(&mut self, ctx: &StopContext) -> LifecycleResult {
        self.stop_activation(ctx).await
    }
    /// `Erase` deletes exactly this sink's rows (`owner`) from the progress
    /// table, idempotently; a missing table is success. `Preserve` keeps them.
    async fn dispose(&mut self, ctx: &DisposeContext) -> LifecycleResult {
        self.delivery = None;
        if ctx.erases_owned_state() {
            self.erase(ctx).await.map_err(lifecycle_error)?;
        }
        self.last_authentication = None;
        Ok(())
    }
}

#[async_trait]
impl DeliveryHandler for MsSqlStoredProcSink {
    fn delivery_guarantee(&self) -> drasi_computation_contracts::DeliveryGuarantee {
        self.config.delivery_guarantee
    }

    async fn handle(&mut self, item: DeliveryItem<'_>) -> Result<()> {
        ensure!(
            item.port.as_str() == "in",
            "unknown SQL Server sink input port"
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
            config,
            outputs,
            templates,
            delivery,
            ..
        } = self;
        let delivery = delivery
            .as_mut()
            .context("SQL Server sink is not running")?;
        let mut render = || Self::render(config, outputs, templates, &item);
        // One attempt through the shared protocol, already classified for
        // the graph (which retries transient failures).
        delivery.deliver(&position, &mut render).await.map(drop)
    }
}

fn refused(reason: impl std::fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
    }
}

#[async_trait]
impl NativeManagement for MsSqlStoredProcSink {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        management::assess(request)
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        let current = MsSqlStoredProcConfig::parse(update.request.current.configuration.clone())
            .map_err(refused)?;
        let proposed = MsSqlStoredProcConfig::parse(update.request.proposed.configuration.clone())
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
        proposed
            .validate_credentials(self.identity_bound)
            .map_err(refused)?;
        let outputs = SinkOutputs::from_inputs(&update.request.proposed.inputs, &proposed.outputs)
            .map_err(refused)?;
        proposed.validate_outputs(&outputs).map_err(refused)?;
        let templates = compile_templates(&proposed).map_err(refused)?;
        match boundary {
            UpdateBoundary::Pause => {
                if self.running() && !self.paused {
                    return Err(refused("in-place updates require a completed pause"));
                }
                let reconnect = management::changes_connection_settings(&self.config, &proposed);
                if let (Some(delivery), Some(authentication)) =
                    (self.delivery.as_mut(), self.last_authentication.as_ref())
                {
                    delivery.set_max_streams(proposed.max_streams);
                    if reconnect {
                        // The idle connection closes; the next operation
                        // reconnects with the new timeouts.
                        let _ = delivery.close(self.config.shutdown_timeout()).await;
                        *delivery.connector_mut() =
                            TiberiusConnector::new(&proposed, authentication, self.owner.clone())
                                .map_err(refused)?;
                    }
                }
            }
            UpdateBoundary::Stop if self.running() => {
                return Err(refused("restart updates require a stopped sink"));
            }
            UpdateBoundary::Stop => {}
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
        }
        self.config = proposed;
        self.outputs = outputs;
        self.templates = templates;
        Ok(())
    }
}
