// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The native MySQL stored-procedure delivery runtime.
//!
//! The sink is a host `DeliveryRunner` consumer (`ConsumerMode::External`).
//! Each operation of an input runs in its own MySQL transaction that commits
//! the procedure effect together with the operation's destination cursor row
//! (see [`super::delivery`]). One activation-owned session carries every
//! operation; a session that may still run an unfinished command is retired
//! with the session KILL proof before any replacement runs.

use super::{
    config::MySqlStoredProcConfig,
    delivery::{self, Effect, Failure, MySqlDeliveryError, Operation, ProgressTable},
    render::Commands,
    session::{self, ConnectPolicy, Endpoint, Sessions},
};
use crate::storedproc;
use anyhow::{Context, Result};
use drasi_computation_contracts::{
    lifecycle::{CancellationToken, StopMode},
    ChangeRoute, ComponentDescriptor, ComponentId, DeliveryBatch, DeliveryItem, DeliveryScope,
    EnvelopeCodec, GraphChangeBatch, GraphChangeCodec, InputBinding, InputEnvelope,
    PipeRequirements, PortDescriptor, PortDirection, PortId, SinkOutputs,
};
use drasi_reaction_common::OperationType;
use std::{num::NonZeroUsize, sync::Arc, time::Duration};
use tokio::time::Instant;

/// Bound used when the sink itself splits a whole input (direct use only).
const MAX_MESSAGE_BYTES: usize = 64 << 20;

/// Stop or Dispose could not prove an owned session gone before its
/// deadline; the owner keeps it and the hook may be retried.
#[derive(Debug)]
pub struct CleanupPending(pub anyhow::Error);

impl std::fmt::Display for CleanupPending {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "MySQL session cleanup is pending: {:#}", self.0)
    }
}

impl std::error::Error for CleanupPending {}

/// Native MySQL stored-procedure consumer runtime.
pub struct MySqlStoredProcSink {
    descriptor: ComponentDescriptor,
    config: Arc<MySqlStoredProcConfig>,
    outputs: SinkOutputs,
    scope: DeliveryScope,
    owner: String,
    table: ProgressTable,
    commands: Commands,
    activation_credentials_bound: bool,
    /// The last activation's account, kept in memory (never serialized) so
    /// cleanup and `Erase` can reach the destination after Stop.
    endpoint: Option<Endpoint>,
    sessions: Sessions,
    started: bool,
    paused: bool,
    table_ready: bool,
}

impl MySqlStoredProcSink {
    /// `inputs` are the graph's declared incoming edges; `scope` is the
    /// delivery scope (construction scope, graph and this component).
    pub fn new(
        id: ComponentId,
        config: MySqlStoredProcConfig,
        inputs: &[InputBinding],
        scope: DeliveryScope,
    ) -> Result<Self> {
        Self::new_with_activation_credentials(id, config, false, inputs, scope)
    }

    pub fn new_with_activation_credentials(
        id: ComponentId,
        config: MySqlStoredProcConfig,
        activation_credentials_bound: bool,
        inputs: &[InputBinding],
        scope: DeliveryScope,
    ) -> Result<Self> {
        config.validate_with_activation_credentials(activation_credentials_bound)?;
        anyhow::ensure!(
            scope.consumer() == &id,
            "MySQL sink delivery scope names another component"
        );
        let outputs = SinkOutputs::from_inputs(inputs, &config.outputs)?;
        config.validate_outputs(&outputs)?;
        let commands = Commands::compile(&config)?;
        let owner = delivery::owner_id(scope.graph(), id.as_str());
        let table = config.progress()?;
        log::debug!(
            "MySQL stored-procedure sink {id} owns cursor rows with owner {owner} in {}",
            table.quoted()
        );
        Ok(Self {
            descriptor: Self::describe(id)?,
            config: Arc::new(config),
            outputs,
            scope,
            owner,
            table,
            commands,
            activation_credentials_bound,
            endpoint: None,
            sessions: Sessions::default(),
            started: false,
            paused: false,
            table_ready: false,
        })
    }

    pub fn ports() -> Result<Vec<PortDescriptor>> {
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

    pub fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    pub fn config(&self) -> &MySqlStoredProcConfig {
        &self.config
    }

    /// The cursor owner of this component in its graph.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(self.config.as_ref())?)
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    fn policy(&self) -> ConnectPolicy {
        ConnectPolicy::from_config(&self.config)
    }

    fn command_timeout(&self) -> Duration {
        Duration::from_millis(self.config.command_timeout_ms)
    }

    fn shutdown_timeout(&self) -> Duration {
        Duration::from_millis(self.config.shutdown_timeout_ms)
    }

    fn cleanup_budget(&self, deadline: Option<Instant>) -> Duration {
        deadline.map_or(self.shutdown_timeout(), |deadline| {
            deadline.saturating_duration_since(Instant::now())
        })
    }

    /// Opens the activation session and creates/verifies the cursor table,
    /// within the start deadline and cancellation. Nothing stays open when it
    /// fails or is cancelled.
    pub async fn start(
        &mut self,
        credentials: Option<(String, String)>,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<()> {
        anyhow::ensure!(!self.started, "MySQL sink is already started");
        if self.sessions.cleanup_pending() {
            let endpoint = self
                .endpoint
                .clone()
                .context("MySQL sink has pending cleanup without an endpoint")?;
            let budget = self.cleanup_budget(deadline);
            self.sessions
                .retire(&endpoint, budget)
                .await
                .context("MySQL sink requires completed cleanup before restart")?;
        }
        let endpoint = match (self.activation_credentials_bound, credentials) {
            (true, Some((user, password))) => Endpoint::new(&self.config, user, password)?,
            (true, None) => anyhow::bail!("MySQL sink requires its bound activation credentials"),
            (false, Some(_)) => anyhow::bail!("MySQL sink has no activation credential binding"),
            (false, None) => Endpoint::new(
                &self.config,
                self.config.user.clone(),
                self.config.password.clone(),
            )?,
        };
        let opened = async {
            let mut live =
                session::connect(&endpoint, self.policy(), Some(cancellation), deadline).await?;
            let ensured =
                delivery::ensure_table(&mut live.connection, &self.table, self.command_timeout());
            let ensured = match deadline {
                Some(deadline) => tokio::time::timeout_at(deadline, ensured)
                    .await
                    .context("MySQL sink start deadline expired")
                    .and_then(|result| result),
                None => ensured.await,
            };
            match ensured {
                Ok(()) => Ok(live),
                Err(error) => {
                    let _ =
                        tokio::time::timeout(Duration::from_secs(1), live.connection.close()).await;
                    Err(error)
                }
            }
        };
        let mut live = tokio::select! {
            biased;
            _ = cancellation.cancelled() => anyhow::bail!("MySQL sink start was cancelled"),
            result = opened => result?,
        };
        live.idle = true;
        self.sessions.live = Some(live);
        self.endpoint = Some(endpoint);
        self.table_ready = true;
        self.started = true;
        self.paused = false;
        Ok(())
    }

    /// Host-driven pause (D-6): the runner calls no `handle` while paused
    /// and has settled the in-flight one.
    pub fn pause(&mut self) {
        self.paused = true;
    }

    pub fn resume(&mut self) {
        self.paused = false;
    }

    /// Closes the activation session. The host owns the in-flight `handle`:
    /// `Drain` awaited it, `Abort` dropped it. A session left mid-command is
    /// KILLed through the ownership proof within the deadline; on expiry the
    /// owner keeps it and returns [`CleanupPending`].
    pub async fn stop(&mut self, mode: StopMode, deadline: Option<Instant>) -> Result<()> {
        self.started = false;
        self.paused = false;
        if self.sessions.live.as_ref().is_some_and(|live| live.busy) {
            log::debug!("MySQL sink stop ({mode:?}) retires a session left mid-command");
            self.sessions.retire_live();
        }
        let budget = self.cleanup_budget(deadline);
        if self.sessions.cleanup_pending() {
            let endpoint = self
                .endpoint
                .clone()
                .context("MySQL sink has pending cleanup without an endpoint")?;
            self.sessions
                .retire(&endpoint, budget)
                .await
                .map_err(|error| anyhow::Error::new(CleanupPending(error)))?;
        }
        self.sessions
            .close_idle(budget.min(Duration::from_secs(5)))
            .await;
        Ok(())
    }

    /// Final disposal. With `erase`, deletes this owner's cursor rows
    /// (idempotent; an absent table has nothing to erase) before closing.
    pub async fn dispose(&mut self, erase: bool, deadline: Option<Instant>) -> Result<()> {
        let budget = self.cleanup_budget(deadline);
        if erase {
            let endpoint = match &self.endpoint {
                Some(endpoint) => endpoint.clone(),
                None if !self.activation_credentials_bound => Endpoint::new(
                    &self.config,
                    self.config.user.clone(),
                    self.config.password.clone(),
                )?,
                None => anyhow::bail!(
                    "MySQL sink cannot erase cursor rows for owner {} without activation \
                     credentials from a previous Start; delete them manually (see CONFIGURATION.md)",
                    self.owner
                ),
            };
            if self.sessions.live.as_ref().is_some_and(|live| live.busy) {
                self.sessions.retire_live();
            }
            self.sessions
                .retire(&endpoint, budget)
                .await
                .map_err(|error| anyhow::Error::new(CleanupPending(error)))?;
            let mut live = match self.sessions.live.take() {
                Some(live) => live,
                None => {
                    session::connect(
                        &endpoint,
                        self.policy(),
                        None,
                        Some(Instant::now() + budget),
                    )
                    .await?
                }
            };
            let erased = delivery::erase(
                &mut live.connection,
                &self.table,
                &self.owner,
                self.command_timeout(),
            )
            .await;
            let _ = tokio::time::timeout(Duration::from_secs(5), live.connection.close()).await;
            erased.context("erasing this component's delivery cursor rows")?;
        }
        if self.sessions.live.as_ref().is_some_and(|live| live.busy) {
            self.sessions.retire_live();
        }
        if let Some(endpoint) = self.endpoint.clone() {
            self.sessions
                .retire(&endpoint, budget)
                .await
                .map_err(|error| anyhow::Error::new(CleanupPending(error)))?;
        }
        self.sessions
            .close_idle(budget.min(Duration::from_secs(5)))
            .await;
        self.started = false;
        Ok(())
    }

    /// Replaces the configuration at a host-established boundary. Templates,
    /// routes, timeouts and retry settings apply to the next operation;
    /// connection settings apply at the next Start.
    pub fn reconfigure(&mut self, config: MySqlStoredProcConfig) -> Result<()> {
        config.validate_with_activation_credentials(self.activation_credentials_bound)?;
        anyhow::ensure!(
            config.outputs == self.config.outputs && config.progress()? == self.table,
            "outputs and progressTable cannot change on a live object"
        );
        config.validate_outputs(&self.outputs)?;
        self.commands = Commands::compile(&config)?;
        self.config = Arc::new(config);
        Ok(())
    }

    /// Host retry classification for a failed operation.
    pub fn retryable(&self, error: &anyhow::Error) -> bool {
        delivery::retryable(error)
    }

    /// The rendered command for one operation; `None` when no template is
    /// configured or a condition excluded the command.
    fn effect(&self, item: &DeliveryItem<'_>) -> Result<Option<Effect>> {
        let output = self.outputs.output_for(item.envelope.system().stream())?;
        let notification =
            GraphChangeBatch::operation_notification(item.envelope, item.operation, output)?;
        let operation = match notification.change().route() {
            ChangeRoute::Added => OperationType::Add,
            ChangeRoute::Updated => OperationType::Update,
            ChangeRoute::Deleted => OperationType::Delete,
        };
        let Some(template) = self.config.template(output.alias, operation) else {
            log::debug!(
                "MySQL sink: no {operation:?} command configured for output {}",
                output.alias
            );
            return Ok(None);
        };
        let rendered = notification
            .to_json()
            .map_err(anyhow::Error::from)
            .and_then(|context| self.commands.render(template, &context))
            .map_err(|error| anyhow::Error::new(MySqlDeliveryError::Render(error)))?;
        if rendered.sql.is_empty() {
            log::debug!("MySQL command condition excluded output {}", output.alias);
            return Ok(None);
        }
        Ok(Some(Effect {
            sql: rendered.sql,
            parameters: rendered
                .parameters
                .iter()
                .map(storedproc::parameter)
                .collect(),
        }))
    }

    async fn retire(&mut self) -> Result<()> {
        self.sessions.retire_live();
        let endpoint = self
            .endpoint
            .clone()
            .context("MySQL sink has no active endpoint")?;
        let budget = self.shutdown_timeout();
        self.sessions.retire(&endpoint, budget).await
    }

    /// Retires the live session after a session-level failure and types the
    /// failure for the host: retryable only once the session is proven gone.
    async fn session_failure(
        &mut self,
        error: anyhow::Error,
        wrap: fn(anyhow::Error) -> MySqlDeliveryError,
    ) -> anyhow::Error {
        match self.retire().await {
            Ok(()) => anyhow::Error::new(wrap(error)),
            Err(cleanup) => anyhow::Error::new(MySqlDeliveryError::CleanupPending(cleanup))
                .context(format!("{error:#}")),
        }
    }

    async fn release(&mut self, admission: Option<String>) {
        let Some(name) = admission else { return };
        let timeout = self.command_timeout();
        let released = match self.sessions.live.as_mut() {
            Some(live) => {
                delivery::bounded(
                    timeout,
                    live.connection
                        .execute(&format!("DO RELEASE_LOCK('{name}')")),
                )
                .await
            }
            None => return,
        };
        if let Err(error) = released {
            log::warn!("MySQL admission lock release failed; retiring the session: {error:#}");
            if let Err(error) = self.retire().await {
                log::warn!("MySQL session cleanup is pending: {error:#}");
            }
        }
    }

    /// Delivers one operation exactly once when the procedure does not end
    /// the transaction itself.
    pub async fn handle(&mut self, item: DeliveryItem<'_>) -> Result<()> {
        anyhow::ensure!(self.started, "MySQL sink is not running");
        anyhow::ensure!(item.port.as_str() == "in", "unknown MySQL sink input port");
        let effect = self.effect(&item)?;
        if self.sessions.live.as_ref().is_some_and(|live| live.busy) {
            self.sessions.retire_live();
        }
        if self.sessions.cleanup_pending() {
            let endpoint = self
                .endpoint
                .clone()
                .context("MySQL sink has no active endpoint")?;
            let budget = self.shutdown_timeout();
            self.sessions
                .retire(&endpoint, budget)
                .await
                .map_err(|error| anyhow::Error::new(MySqlDeliveryError::CleanupPending(error)))?;
        }
        for attempt in 0..2 {
            if self.sessions.live.is_none() {
                let endpoint = self
                    .endpoint
                    .clone()
                    .context("MySQL sink has no active endpoint")?;
                let mut live = session::connect(&endpoint, self.policy(), None, None).await?;
                if !self.table_ready {
                    delivery::ensure_table(
                        &mut live.connection,
                        &self.table,
                        self.command_timeout(),
                    )
                    .await?;
                    self.table_ready = true;
                }
                self.sessions.live = Some(live);
            }
            let operation = Operation {
                table: &self.table,
                owner: &self.owner,
                max_streams: self.config.max_streams,
                command_timeout: Duration::from_millis(self.config.command_timeout_ms),
            };
            let live = self
                .sessions
                .live
                .as_mut()
                .context("MySQL session missing")?;
            let reused = live.idle;
            live.busy = true;
            live.idle = false;
            let mut admission = None;
            let result = operation
                .run(&mut live.connection, &item, effect.as_ref(), &mut admission)
                .await;
            match result {
                Ok(_) => {
                    live.busy = false;
                    self.release(admission).await;
                    if let Some(live) = self.sessions.live.as_mut() {
                        live.idle = true;
                    }
                    return Ok(());
                }
                Err(Failure::NotStarted(error)) if delivery::breaks_session(&error) => {
                    let lost = self
                        .session_failure(error, MySqlDeliveryError::ConnectionLost)
                        .await;
                    if reused && attempt == 0 && !self.sessions.cleanup_pending() {
                        log::info!("MySQL idle session was gone; reconnecting: {lost:#}");
                        continue;
                    }
                    return Err(lost);
                }
                Err(Failure::NotStarted(error)) => {
                    live.busy = false;
                    live.idle = true;
                    return Err(error);
                }
                Err(Failure::InTransaction(error)) if delivery::breaks_session(&error) => {
                    let wrap = if delivery::timed_out(&error) {
                        MySqlDeliveryError::CommandTimeout
                    } else {
                        MySqlDeliveryError::ConnectionLost
                    };
                    return Err(self.session_failure(error, wrap).await);
                }
                Err(Failure::InTransaction(error)) => {
                    let rolled_back = delivery::bounded(
                        operation.command_timeout,
                        live.connection.execute("ROLLBACK"),
                    )
                    .await;
                    match rolled_back {
                        Ok(()) => {
                            live.busy = false;
                            live.idle = true;
                            self.release(admission).await;
                            return Err(error);
                        }
                        Err(rollback) => {
                            log::warn!("MySQL ROLLBACK failed; retiring the session: {rollback:#}");
                            return match self.retire().await {
                                Ok(()) => {
                                    Err(error
                                        .context("the session was retired after ROLLBACK failed"))
                                }
                                Err(cleanup) => Err(anyhow::Error::new(
                                    MySqlDeliveryError::CleanupPending(cleanup),
                                )
                                .context(format!("{error:#}"))),
                            };
                        }
                    }
                }
                Err(Failure::Commit(error)) => {
                    return Err(self
                        .session_failure(error, MySqlDeliveryError::CommitUnknown)
                        .await);
                }
            }
        }
        anyhow::bail!("MySQL operation was not delivered")
    }

    /// Delivers every operation of one input in order (direct use outside a
    /// host `DeliveryRunner`). The destination cursor skips the completed
    /// prefix, so retrying the same input repeats no committed operation.
    pub async fn handle_input(&mut self, input: InputEnvelope) -> Result<()> {
        let operations = input.envelope.changes().operations().len();
        GraphChangeBatch::check_envelope(&input.envelope)?;
        let mut codec =
            EnvelopeCodec::new(NonZeroUsize::new(MAX_MESSAGE_BYTES).context("message bound")?);
        codec.register_schema(GraphChangeCodec::schema())?;
        let batch = DeliveryBatch::new(self.scope.clone(), input, &codec)?;
        for index in 0..operations {
            self.handle(batch.item(index)?).await?;
        }
        Ok(())
    }
}

impl Drop for MySqlStoredProcSink {
    fn drop(&mut self) {
        if self.sessions.abandoned() {
            log::warn!("MySQL stored-procedure sink dropped with an unproven session cleanup");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::{OutputEndpoint, StreamId};
    use drasi_core_types::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    };
    use serde_json::json;

    const OUTPUT_COMPONENT_ID: &str = "68b2acc4-86bd-5b9c-998b-67a11634c870";
    const SINK_ID: &str = "e43a695e-d537-52b4-892b-117de3000c96";

    fn sink() -> Result<MySqlStoredProcSink> {
        let inputs = [InputBinding {
            input_port: PortId::try_new("in")?,
            output: OutputEndpoint::new(
                ComponentId::try_new(OUTPUT_COMPONENT_ID)?,
                PortId::try_new("out")?,
            ),
            stream: StreamId::try_new("facilities-db/out")?,
        }];
        let id = ComponentId::try_new(SINK_ID)?;
        MySqlStoredProcSink::new(
            id.clone(),
            MySqlStoredProcConfig::parse(json!({
                "hostname": "no-connection-at-construction", "database": "d",
                "user": "u", "password": "p",
                "outputs": {"facilities-db": {"component": OUTPUT_COMPONENT_ID, "port": "out"}},
                "defaultTemplate": {
                    "added": {"template": "CALL a({{param operation}},{{param identity.id}},{{param after.name}})"},
                    "updated": {"template": "CALL u({{param operation}},{{param after.name}})"},
                    "deleted": {"template": "CALL d({{param identity.id}},{{param after.name}})"}
                }
            }))?,
            &inputs,
            DeliveryScope::new("instance".into(), "graph".into(), id)?,
        )
    }

    fn metadata() -> ElementMetadata {
        ElementMetadata {
            reference: ElementReference::new(OUTPUT_COMPONENT_ID, "room-1"),
            labels: std::sync::Arc::from([std::sync::Arc::<str>::from("Room")]),
            effective_from: 1_000,
        }
    }

    fn room(name: &str) -> Element {
        let mut properties = ElementPropertyMap::new();
        properties.insert("name", ElementValue::String(name.into()));
        Element::Node {
            metadata: metadata(),
            properties,
        }
    }

    fn input(stream: &str) -> Result<InputEnvelope> {
        let producer: drasi_computation_contracts::GraphProducerIdentity =
            serde_json::from_value(json!({
                "construction_scope": "instance", "graph_id": "graph",
                "component_id": OUTPUT_COMPONENT_ID, "stream": stream,
                "incarnation": "c218b0c6-8803-48df-aa4c-d5350f0e6453", "persistent": true
            }))?;
        let mut envelope = GraphChangeCodec::encode_changes(
            &[
                SourceChange::Insert {
                    element: room("one"),
                },
                SourceChange::Update {
                    element: room("two"),
                    removed_properties: Default::default(),
                },
                SourceChange::Delete {
                    metadata: metadata(),
                },
            ],
            StreamId::try_new(stream)?,
            3,
            None,
        )?;
        drasi_computation_contracts::GraphProducerProgress::annotate(&mut envelope, &producer, 3)?;
        Ok(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope,
        })
    }

    fn effects(
        sink: &MySqlStoredProcSink,
        input: InputEnvelope,
    ) -> Result<Vec<Result<Option<Effect>>>> {
        let mut codec = EnvelopeCodec::new(NonZeroUsize::new(MAX_MESSAGE_BYTES).unwrap());
        codec.register_schema(GraphChangeCodec::schema())?;
        let operations = input.envelope.changes().operations().len();
        let batch = DeliveryBatch::new(sink.scope.clone(), input, &codec)?;
        (0..operations)
            .map(|index| Ok(sink.effect(&batch.item(index)?)))
            .collect()
    }

    #[test]
    fn operations_render_individually_and_a_render_failure_is_permanent() -> Result<()> {
        let sink = sink()?;
        let effects = effects(&sink, input("facilities-db/out")?)?;
        let first = effects[0]
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e:#}"))?
            .as_ref()
            .unwrap();
        assert_eq!(first.sql, "CALL a(?,?,?)");
        assert_eq!(first.parameters.len(), 3);
        let second = effects[1]
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e:#}"))?
            .as_ref()
            .unwrap();
        assert_eq!(second.sql, "CALL u(?,?)");
        // A delete carries no after image, so the template cannot render.
        let error = effects[2]
            .as_ref()
            .err()
            .context("expected render failure")?;
        assert!(matches!(
            error.downcast_ref::<MySqlDeliveryError>(),
            Some(MySqlDeliveryError::Render(_))
        ));
        assert!(!sink.retryable(error));
        Ok(())
    }

    #[test]
    fn unbound_streams_fail_and_owner_is_scoped() -> Result<()> {
        let sink = sink()?;
        assert!(effects(&sink, input("other/out")?)?
            .iter()
            .all(Result::is_err));
        assert_eq!(sink.owner(), delivery::owner_id("graph", SINK_ID));
        Ok(())
    }

    #[tokio::test]
    async fn handle_requires_start_and_credentials_follow_binding() -> Result<()> {
        let mut sink = sink()?;
        assert!(sink
            .handle_input(input("facilities-db/out")?)
            .await
            .is_err());
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(sink.start(None, &cancelled, None).await.is_err());
        assert!(!sink.is_started());
        assert!(sink
            .start(
                Some(("u".into(), "p".into())),
                &CancellationToken::new(),
                None
            )
            .await
            .is_err());
        sink.stop(StopMode::Drain, None).await?;
        Ok(())
    }
}
