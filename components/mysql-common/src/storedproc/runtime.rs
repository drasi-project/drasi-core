// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::config::MySqlStoredProcConfig;
use crate::{
    native::{
        connection::{Connection, MySqlServerError},
        session::{server_id, SessionCleanup},
    },
    storedproc::{self, CommandRenderer, RenderedCommand},
};
use anyhow::{Context, Result};
use drasi_computation_contracts::{
    ChangeRoute, ComponentDescriptor, ComponentId, GraphChangeBatch, GraphChangeCodec,
    InputBinding, InputEnvelope, OperationFailures, PipeRequirements, PortDescriptor,
    PortDirection, PortId, SinkCompletion, SinkOutputs,
};
use drasi_reaction_common::OperationType;
use std::{collections::BTreeSet, future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::Mutex;

type Pending = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

/// Success means that MySQL completed the configured commands, not that an
/// external effect and the graph checkpoint form one atomic transaction.
pub struct MySqlStoredProcSink {
    descriptor: ComponentDescriptor,
    config: Arc<MySqlStoredProcConfig>,
    outputs: SinkOutputs,
    activation_credentials_bound: bool,
    active_config: Option<Arc<MySqlStoredProcConfig>>,
    pending: Mutex<Option<Pending>>,
    started: bool,
    interrupted: bool,
    cleanup_required: bool,
}

impl MySqlStoredProcSink {
    /// `inputs` are the graph's declared incoming edges; they define the
    /// outputs this sink accepts.
    pub fn new(
        id: ComponentId,
        config: MySqlStoredProcConfig,
        inputs: &[InputBinding],
    ) -> Result<Self> {
        Self::new_with_activation_credentials(id, config, false, inputs)
    }
    pub fn new_with_activation_credentials(
        id: ComponentId,
        config: MySqlStoredProcConfig,
        activation_credentials_bound: bool,
        inputs: &[InputBinding],
    ) -> Result<Self> {
        config.validate_with_activation_credentials(activation_credentials_bound)?;
        let outputs = SinkOutputs::from_inputs(inputs, &config.outputs)?;
        config.validate_outputs(&outputs)?;
        Ok(Self {
            descriptor: Self::describe(id)?,
            config: Arc::new(config),
            outputs,
            activation_credentials_bound,
            active_config: None,
            pending: Mutex::new(None),
            started: false,
            interrupted: false,
            cleanup_required: false,
        })
    }
    pub fn describe(id: ComponentId) -> Result<ComponentDescriptor> {
        Ok(ComponentDescriptor::try_new(
            id,
            vec![PortDescriptor::new(
                PortId::try_new("in")?,
                PortDirection::Input,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?)
    }
    async fn finish(&mut self) -> Result<()> {
        let result = self
            .pending
            .get_mut()
            .as_mut()
            .context("MySQL operation is missing")?
            .await;
        *self.pending.get_mut() = None;
        result
    }
    fn commands(&self, input: &InputEnvelope) -> Result<Vec<RenderedCommand>> {
        anyhow::ensure!(input.port.as_str() == "in", "unknown MySQL sink input");
        let output = self.outputs.output_for(input.envelope.system().stream())?;
        let batch = GraphChangeBatch::from_envelope(&input.envelope)?;
        anyhow::ensure!(
            batch.changes().len() <= 4096,
            "MySQL graph change set exceeds operation bound"
        );
        let mut commands = Vec::new();
        let mut bytes = 0usize;
        for notification in batch.notifications(output) {
            let operation = match notification.change().route() {
                ChangeRoute::Added => OperationType::Add,
                ChangeRoute::Updated => OperationType::Update,
                ChangeRoute::Deleted => OperationType::Delete,
            };
            let Some(template) = self.config.template(output.alias, operation) else {
                log::debug!(
                    "MySQL sink: no {:?} command configured for output {}",
                    operation,
                    output.alias
                );
                continue;
            };
            // The context is the truthful change: operation, identity, sequence
            // and only the images the record carries.
            let context = notification.to_json()?;
            let command = CommandRenderer::new().render(template, &context)?;
            if command.sql.is_empty() {
                log::debug!("MySQL command condition excluded output {}", output.alias);
                continue;
            }
            let parameter_bytes = serde_json::to_vec(&command.params)?.len();
            bytes = bytes
                .checked_add(command.sql.len())
                .and_then(|bytes| bytes.checked_add(parameter_bytes))
                .context("MySQL command size overflow")?;
            anyhow::ensure!(
                bytes <= self.config.max_protocol_bytes.get(),
                "MySQL commands exceed byte limit"
            );
            commands.push(command);
        }
        Ok(commands)
    }
}

async fn connect(config: &MySqlStoredProcConfig) -> Result<Connection> {
    let settings = config.connection();
    for attempt in 0..=config.retry_attempts {
        let connected = tokio::time::timeout(
            Duration::from_millis(config.connect_timeout_ms.get()),
            crate::native::session::connect(
                &settings,
                config.max_protocol_bytes.get(),
                Duration::from_millis(config.command_timeout_ms.get()),
            ),
        )
        .await
        .context("MySQL connection deadline")
        .and_then(|result| result);
        match connected {
            Ok(connection) => return Ok(connection),
            Err(error) => {
                let retryable = error.downcast_ref::<std::io::Error>().is_some()
                    || error
                        .downcast_ref::<tokio::time::error::Elapsed>()
                        .is_some()
                    || error
                        .downcast_ref::<MySqlServerError>()
                        .is_some_and(|error| matches!(error.code, 1040 | 1203));
                if !retryable || attempt == config.retry_attempts {
                    return Err(error);
                }
                log::warn!(
                    "MySQL connection attempt failed before any procedure execution: {error:#}"
                );
                tokio::time::sleep(Duration::from_millis(100 * (1u64 << attempt))).await;
            }
        }
    }
    unreachable!("bounded connection loop returns on its last attempt")
}

async fn execute(config: Arc<MySqlStoredProcConfig>, commands: Vec<RenderedCommand>) -> Result<()> {
    let mut control = None;
    let mut sessions = Vec::new();
    let mut server = None;
    let mut killed = BTreeSet::new();
    let operation = async {
        control = Some(connect(&config).await?);
        server = Some(server_id(control.as_mut().context("MySQL control connection")?).await?);
        if commands.is_empty() {
            return Ok(());
        }
        let mut connection = connect(&config).await?;
        let identity = server_id(&mut connection).await?;
        if Some(identity) != server {
            connection.close().await?;
            anyhow::bail!("MySQL procedure connection reached another server");
        }
        sessions.push(connection);
        for command in commands {
            let parameters = command
                .params
                .iter()
                .map(storedproc::parameter)
                .collect::<Vec<_>>();
            sessions[0]
                .execute_parameters(&command.sql, &parameters)
                .await?;
        }
        Result::<()>::Ok(())
    }
    .await;
    let settings = config.connection();
    let cleanup = SessionCleanup {
        config: &settings,
        max_bytes: config.max_protocol_bytes.get(),
        timeout: Duration::from_millis(config.command_timeout_ms.get()),
        control: &mut control,
        sessions: &mut sessions,
        server: &server,
        killed: &mut killed,
    }
    .cleanup()
    .await;
    match (operation, cleanup) {
        (Err(primary), Err(cleanup)) => Err(OperationFailures::new(
            "MySQL procedure and cleanup failed",
            vec![primary, cleanup],
        )
        .into()),
        (Err(error), _) | (_, Err(error)) => Err(error),
        _ => Ok(()),
    }
}

impl MySqlStoredProcSink {
    pub fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    pub fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({"settings":self.config.as_ref()}))
    }
    pub async fn start(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.activation_credentials_bound,
            "MySQL sink requires its bound activation credentials"
        );
        self.start_activation(self.config.clone()).await
    }
    pub async fn start_with_credentials(
        &mut self,
        username: String,
        password: String,
    ) -> Result<()> {
        anyhow::ensure!(
            self.activation_credentials_bound,
            "MySQL sink has no activation credential binding"
        );
        let mut config = self.config.as_ref().clone();
        config.user = username;
        config.password = password;
        config.validate()?;
        self.start_activation(Arc::new(config)).await
    }
    async fn start_activation(&mut self, config: Arc<MySqlStoredProcConfig>) -> Result<()> {
        anyhow::ensure!(
            !self.cleanup_required && self.pending.get_mut().is_none(),
            "MySQL sink requires awaited cleanup"
        );
        self.cleanup_required = true;
        self.active_config = Some(config.clone());
        *self.pending.get_mut() = Some(Box::pin(execute(config, Vec::new())));
        self.finish().await?;
        self.started = true;
        self.interrupted = false;
        Ok(())
    }
    pub async fn stop(&mut self) -> Result<()> {
        self.started = false;
        if self.pending.get_mut().is_some() {
            let timeout = Duration::from_millis(self.config.shutdown_timeout_ms.get());
            tokio::time::timeout(timeout, self.finish())
                .await
                .context("MySQL cleanup is pending; retry stop")??;
        }
        self.interrupted = false;
        self.cleanup_required = false;
        self.active_config = None;
        Ok(())
    }
}
impl MySqlStoredProcSink {
    pub fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    pub async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        anyhow::ensure!(
            self.started && !self.interrupted && self.pending.get_mut().is_none(),
            "MySQL sink is stopped or needs cleanup after an interrupted operation"
        );
        let commands = self.commands(&input)?;
        if commands.is_empty() {
            return Ok(());
        }
        self.interrupted = true;
        let config = self
            .active_config
            .as_ref()
            .context("MySQL sink has no active authentication")?
            .clone();
        *self.pending.get_mut() = Some(Box::pin(execute(config, commands)));
        let result = self.finish().await;
        if result.is_ok() {
            self.interrupted = false;
        }
        result
    }
}
impl Drop for MySqlStoredProcSink {
    fn drop(&mut self) {
        if self.pending.get_mut().is_some() {
            log::warn!("MySQL procedure sink dropped without awaited cleanup");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::StreamId;
    use drasi_core_types::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    };
    use serde_json::json;

    fn sink() -> Result<MySqlStoredProcSink> {
        let inputs = [InputBinding {
            input_port: PortId::try_new("in")?,
            output: drasi_computation_contracts::OutputEndpoint::new(
                ComponentId::try_new("facilities-db")?,
                PortId::try_new("out")?,
            ),
            stream: StreamId::try_new("facilities-db/out")?,
        }];
        MySqlStoredProcSink::new(
            ComponentId::try_new("sink")?,
            serde_json::from_value(json!({
                "hostname": "no-connection-at-construction", "database": "d",
                "user": "u", "password": "p",
                "outputs": {"facilities-db": {"component": "facilities-db", "port": "out"}},
                "default_template": {
                    "added": {"template": "CALL a({{param operation}},{{param identity.id}},{{param after.name}})"},
                    "updated": {"template": "CALL u({{param operation}},{{param after.name}})"},
                    "deleted": {"template": "CALL d({{param identity.id}})"}
                }
            }))?,
            &inputs,
        )
    }

    fn metadata() -> ElementMetadata {
        ElementMetadata {
            reference: ElementReference::new("facilities-db", "room-1"),
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
        Ok(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: GraphChangeCodec::encode_changes(
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
            )?,
        })
    }

    #[test]
    fn source_graph_changes_render_ordered_commands_without_query_annotations() -> Result<()> {
        let commands = sink()?.commands(&input("facilities-db/out")?)?;
        assert_eq!(
            commands
                .iter()
                .map(|command| command.sql.as_str())
                .collect::<Vec<_>>(),
            ["CALL a(?,?,?)", "CALL u(?,?)", "CALL d(?)"]
        );
        assert_eq!(
            commands[0].params,
            [json!("ADD"), json!("room-1"), json!("one")]
        );
        assert_eq!(commands[1].params, [json!("PATCH"), json!("two")]);
        assert_eq!(commands[2].params, [json!("room-1")]);
        Ok(())
    }

    #[test]
    fn unbound_streams_and_scheduling_controls_fail_instead_of_being_dropped() -> Result<()> {
        assert!(sink()?.commands(&input("other/out")?).is_err());
        let due = InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: GraphChangeCodec::encode_futures_due(
                StreamId::try_new("facilities-db/out")?,
                4,
                chrono::Utc::now(),
            )?,
        };
        assert!(sink()?.commands(&due).is_err());
        Ok(())
    }
}
