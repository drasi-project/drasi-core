// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The native MySQL stored-procedure delivery runtime.
//!
//! The sink is a host `DeliveryRunner` consumer (`ConsumerMode::External`).
//! Each operation of an input runs in its own MySQL transaction that commits
//! the procedure effect together with the operation's destination cursor row,
//! through the protocol shared by every stored-procedure sink
//! (`drasi_computation_storedproc_common::driver`) over a [`MySqlSession`].
//! One activation-owned session carries every operation; a session that may
//! still run an unfinished command is retired with the session KILL proof
//! before any replacement runs.

use super::{
    config::MySqlStoredProcConfig,
    delivery::{
        self, CleanupPending, Effect, MySqlConnector, MySqlSession, ProgressTable, Timeouts,
    },
    render::Commands,
    session::{self, Endpoint},
};
use crate::storedproc;
use anyhow::{Context, Result};
use drasi_computation_contracts::{
    lifecycle::{CancellationToken, StopMode},
    ComponentDescriptor, ComponentId, DeliveryBatch, DeliveryGuarantee, DeliveryItem,
    DeliveryScope, EnvelopeCodec, GraphChangeBatch, GraphChangeCodec, InputBinding, InputEnvelope,
    PipeRequirements, PortDescriptor, PortDirection, PortId, SinkOutputs,
};
use drasi_computation_storedproc_common::{
    driver::{Delivery, Session},
    OperationContext, Position,
};
use std::{num::NonZeroUsize, sync::Arc, time::Duration};
use tokio::time::Instant;

/// Bound used when the sink itself splits a whole input (direct use only).
const MAX_MESSAGE_BYTES: usize = 64 << 20;

/// A template that failed to render for this change (permanent).
#[derive(Debug, thiserror::Error)]
#[error("MySQL command template failed to render: {0:#}")]
pub struct RenderError(pub anyhow::Error);

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
    /// The activation's delivery engine. Kept after Stop while a retired
    /// session's KILL is not yet proven, so cleanup can finish later.
    delivery: Option<Delivery<MySqlConnector>>,
    started: bool,
    paused: bool,
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
        let owner = drasi_computation_storedproc_common::owner(scope.graph(), id.as_str());
        let table = config.progress()?;
        log::debug!(
            "MySQL stored-procedure sink {id} owns cursor rows with owner {owner} in {}",
            delivery::quoted(&table, &config.database)
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
            delivery: None,
            started: false,
            paused: false,
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

    fn timeouts(config: &MySqlStoredProcConfig) -> Timeouts {
        Timeouts {
            connect: Duration::from_millis(config.connect_timeout_ms),
            command: Duration::from_millis(config.command_timeout_ms),
            shutdown: Duration::from_millis(config.shutdown_timeout_ms),
        }
    }

    fn shutdown_timeout(&self) -> Duration {
        Duration::from_millis(self.config.shutdown_timeout_ms)
    }

    fn cleanup_budget(&self, deadline: Option<Instant>) -> Duration {
        deadline.map_or(self.shutdown_timeout(), |deadline| {
            deadline.saturating_duration_since(Instant::now())
        })
    }

    fn delivery(&self, endpoint: Endpoint) -> Result<Delivery<MySqlConnector>> {
        Ok(Delivery::new(
            MySqlConnector::new(
                endpoint,
                self.owner.clone(),
                self.config.database.clone(),
                self.table.clone(),
                Self::timeouts(&self.config),
            )?,
            self.config.max_streams,
            self.config.on_permanent_failure,
        ))
    }

    /// Proves sessions retired by an earlier activation gone.
    async fn finish_cleanup(&mut self, budget: Duration) -> Result<()> {
        if let Some(delivery) = self.delivery.as_mut() {
            if delivery.connector().cleanup_pending() {
                delivery.connector_mut().retire(budget).await?;
            }
        }
        Ok(())
    }

    /// Opens the activation session and creates/verifies the cursor tables:
    /// one attempt within `connectTimeoutMs`, the start deadline and
    /// cancellation. Nothing stays open when it fails or is cancelled.
    pub async fn start(
        &mut self,
        credentials: Option<(String, String)>,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<()> {
        anyhow::ensure!(!self.started, "MySQL sink is already started");
        let budget = self.cleanup_budget(deadline);
        self.finish_cleanup(budget)
            .await
            .context("MySQL sink requires completed cleanup before restart")?;
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
        let mut delivery = self.delivery(endpoint.clone())?;
        let prepared = async {
            match deadline {
                Some(deadline) => tokio::time::timeout_at(deadline, delivery.prepare())
                    .await
                    .context("MySQL sink start deadline expired")?,
                None => delivery.prepare().await,
            }
        };
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => anyhow::bail!("MySQL sink start was cancelled"),
            result = prepared => result?,
        }
        self.delivery = Some(delivery);
        self.endpoint = Some(endpoint);
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
        let budget = self.cleanup_budget(deadline);
        let Some(delivery) = self.delivery.as_mut() else {
            return Ok(());
        };
        if delivery.abandoned() {
            log::debug!("MySQL sink stop ({mode:?}) retires a session left mid-command");
        }
        if let Err(error) = delivery.close(budget.min(Duration::from_secs(5))).await {
            log::debug!("MySQL session close reported: {error:#}");
        }
        if delivery.connector().cleanup_pending() {
            delivery.connector_mut().retire(budget).await?;
        }
        self.delivery = None;
        Ok(())
    }

    /// Final disposal. With `erase`, deletes this owner's cursor and
    /// dead-letter rows (idempotent; absent tables have nothing to erase)
    /// before closing.
    pub async fn dispose(&mut self, erase: bool, deadline: Option<Instant>) -> Result<()> {
        let budget = self.cleanup_budget(deadline);
        self.finish_cleanup(budget).await?;
        if let Some(mut delivery) = self.delivery.take() {
            let _ = delivery.close(budget.min(Duration::from_secs(5))).await;
            if delivery.connector().cleanup_pending() {
                delivery.connector_mut().retire(budget).await?;
            }
        }
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
            let mut delivery = self.delivery(endpoint)?;
            let erased = tokio::time::timeout(budget, delivery.erase())
                .await
                .context("erasing this component's delivery cursor rows timed out")
                .and_then(|result| result);
            let _ = delivery.close(Duration::from_secs(5)).await;
            erased.context("erasing this component's delivery cursor rows")?;
        }
        self.started = false;
        Ok(())
    }

    /// Replaces the configuration at a host-established boundary. Templates,
    /// routes, timeouts and `maxStreams` apply to the next operation;
    /// connection settings apply at the next Start.
    pub fn reconfigure(&mut self, config: MySqlStoredProcConfig) -> Result<()> {
        config.validate_with_activation_credentials(self.activation_credentials_bound)?;
        anyhow::ensure!(
            config.outputs == self.config.outputs && config.progress()? == self.table,
            "outputs and progressTable cannot change on a live object"
        );
        config.validate_outputs(&self.outputs)?;
        self.commands = Commands::compile(&config)?;
        if let Some(delivery) = self.delivery.as_mut() {
            delivery.set_max_streams(config.max_streams);
            delivery
                .connector_mut()
                .set_timeouts(Self::timeouts(&config));
        }
        self.config = Arc::new(config);
        Ok(())
    }

    /// The configured guarantee; only `AtLeastOnce` accepts in-memory queries.
    pub fn delivery_guarantee(&self) -> DeliveryGuarantee {
        self.config.delivery_guarantee
    }

    /// The rendered command for one operation; `None` when no template is
    /// configured or a condition excluded the command.
    fn effect(
        config: &MySqlStoredProcConfig,
        outputs: &SinkOutputs,
        commands: &Commands,
        item: &DeliveryItem<'_>,
    ) -> Result<Option<Effect>> {
        let input = OperationContext::new(item, outputs)?;
        let Some(template) = config.template(&input.alias, input.operation) else {
            log::debug!(
                "MySQL sink: no {:?} command configured for output {}",
                input.operation,
                input.alias
            );
            return Ok(None);
        };
        let rendered = commands
            .render(template, &input.context)
            .map_err(|error| anyhow::Error::new(RenderError(error)))?;
        if rendered.sql.is_empty() {
            log::debug!("MySQL command condition excluded output {}", input.alias);
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

    /// Delivers one operation in one transaction with its cursor row: one
    /// attempt, classified for the graph, which retries transient failures
    /// (deadlocks, lock waits, lost sessions); under `onPermanentFailure:
    /// skip` a content failure is settled as skipped.
    pub async fn handle(&mut self, item: DeliveryItem<'_>) -> Result<()> {
        anyhow::ensure!(self.started, "MySQL sink is not running");
        anyhow::ensure!(item.port.as_str() == "in", "unknown MySQL sink input port");
        if item.position.epoch().is_some() && !self.delivery_guarantee().is_at_least_once() {
            // Internal invariant guard only: the graph refuses this pairing when
            // it binds the edge.
            return Err(
                drasi_computation_contracts::Incompatibility::ProducerIdentity {
                    required: DeliveryGuarantee::ExactlyOnce,
                    identity: drasi_computation_contracts::OutputIdentity::Volatile,
                }
                .into(),
            );
        }
        let position = Position::from_item(&item)?;
        let Self {
            config,
            outputs,
            commands,
            delivery,
            ..
        } = self;
        let delivery = delivery.as_mut().context("MySQL sink is not running")?;
        let mut render = || Self::effect(config, outputs, commands, &item);
        delivery.deliver(&position, &mut render).await.map(drop)
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
        let batch = DeliveryBatch::new_with_guarantee(
            self.scope.clone(),
            input,
            &codec,
            self.delivery_guarantee(),
        )?;
        for index in 0..operations {
            self.handle(batch.item(index)?).await?;
        }
        Ok(())
    }

    /// Classifies a delivery error the way the protocol does.
    pub fn classify(error: &anyhow::Error) -> drasi_computation_storedproc_common::ErrorClass {
        MySqlSession::classify(error)
    }
}

impl Drop for MySqlStoredProcSink {
    fn drop(&mut self) {
        if self
            .delivery
            .as_ref()
            .is_some_and(|delivery| delivery.abandoned() || delivery.connector().cleanup_pending())
        {
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
                "host": "no-connection-at-construction", "database": "d",
                "user": "u", "password": "p",
                "outputs": {"facilities-db": {"component": OUTPUT_COMPONENT_ID, "port": "out"}},
                "defaultTemplate": {
                    "added": {"template": "CALL a({{param operation}},{{param identity.id}},{{param after.name}},{{param deliveryId}})"},
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
            execution_context: Default::default(),
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
            .map(|index| {
                Ok(MySqlStoredProcSink::effect(
                    &sink.config,
                    &sink.outputs,
                    &sink.commands,
                    &batch.item(index)?,
                ))
            })
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
        assert_eq!(first.sql, "CALL a(?,?,?,?)");
        assert_eq!(first.parameters.len(), 4);
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
        assert!(error.downcast_ref::<RenderError>().is_some());
        Ok(())
    }

    #[test]
    fn unbound_streams_fail_and_owner_is_scoped() -> Result<()> {
        let sink = sink()?;
        assert!(effects(&sink, input("other/out")?)?
            .iter()
            .all(Result::is_err));
        assert_eq!(
            sink.owner(),
            drasi_computation_storedproc_common::owner("graph", SINK_ID)
        );
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
