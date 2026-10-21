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
    exchange::ResultDiff, ComponentDescriptor, ComponentId, InputEnvelope, OperationFailures,
    PipeRequirements, PortDescriptor, PortDirection, PortId, QueryChangeCodec, SinkCompletion,
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
    activation_credentials_bound: bool,
    active_config: Option<Arc<MySqlStoredProcConfig>>,
    pending: Mutex<Option<Pending>>,
    started: bool,
    interrupted: bool,
    cleanup_required: bool,
}

impl MySqlStoredProcSink {
    pub fn new(id: ComponentId, config: MySqlStoredProcConfig) -> Result<Self> {
        Self::new_with_activation_credentials(id, config, false)
    }
    pub fn new_with_activation_credentials(
        id: ComponentId,
        config: MySqlStoredProcConfig,
        activation_credentials_bound: bool,
    ) -> Result<Self> {
        config.validate_with_activation_credentials(activation_credentials_bound)?;
        Ok(Self {
            descriptor: Self::describe(id)?,
            config: Arc::new(config),
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
                QueryChangeCodec::schema().descriptor().clone(),
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
        let result = QueryChangeCodec::to_legacy_result(&input.envelope)?;
        anyhow::ensure!(
            self.config.queries.contains(&result.query_id),
            "MySQL sink received an unconfigured query"
        );
        anyhow::ensure!(
            result.results.len() <= 4096,
            "MySQL query result exceeds operation bound"
        );
        let timestamp = result.timestamp.to_rfc3339();
        let mut commands = Vec::new();
        let mut bytes = 0usize;
        for diff in &result.results {
            let (operation, before, after, data) = match diff {
                ResultDiff::Add { data, .. } => (OperationType::Add, None, Some(data), None),
                ResultDiff::Update {
                    data,
                    before,
                    after,
                    ..
                } => (OperationType::Update, Some(before), Some(after), Some(data)),
                ResultDiff::Delete { data, .. } => (OperationType::Delete, Some(data), None, None),
                ResultDiff::Aggregation { before, after, .. } => {
                    (OperationType::Update, before.as_ref(), Some(after), None)
                }
                ResultDiff::Noop => continue,
            };
            let Some(template) = self.config.template(&result.query_id, operation) else {
                log::debug!(
                    "MySQL sink: no {:?} command configured for {}",
                    operation,
                    result.query_id
                );
                continue;
            };
            let context = storedproc::build_context(
                &result.query_id,
                &operation.as_str().to_uppercase(),
                &timestamp,
                &result.metadata,
                before,
                after,
                data,
            );
            let command = CommandRenderer::new().render(template, &context)?;
            if command.sql.is_empty() {
                log::debug!("MySQL command condition excluded query {}", result.query_id);
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
