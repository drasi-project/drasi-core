// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! MS SQL Server Stored Procedure reaction implementation.

use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::context::ReactionRuntimeContext;
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::{Reaction, ReactionBase, ReactionBaseParams};

use crate::config::{MsSqlStoredProcReactionConfig, QueryConfig};
use crate::executor::MsSqlExecutor;
use crate::render::{build_context, render_command};

/// MS SQL Server Stored Procedure reaction
///
/// Invokes MS SQL Server stored procedures when continuous query results change.
/// Supports different procedures for ADD, UPDATE, and DELETE operations.
pub struct MsSqlStoredProcReaction {
    pub(crate) base: ReactionBase,
    config: MsSqlStoredProcReactionConfig,
    executor: RwLock<Option<Arc<MsSqlExecutor>>>,
}

impl std::fmt::Debug for MsSqlStoredProcReaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MsSqlStoredProcReaction")
            .field("config", &self.config)
            .finish()
    }
}

impl MsSqlStoredProcReaction {
    /// Create a builder for MsSqlStoredProcReaction
    pub fn builder(id: impl Into<String>) -> MsSqlStoredProcReactionBuilder {
        MsSqlStoredProcReactionBuilder::new(id)
    }

    /// Create a new MS SQL Server stored procedure reaction
    pub async fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: MsSqlStoredProcReactionConfig,
    ) -> Result<Self> {
        Self::create_internal(id.into(), queries, config, None, true).await
    }

    /// Create a new MS SQL Server stored procedure reaction with custom priority queue capacity
    pub async fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: MsSqlStoredProcReactionConfig,
        priority_queue_capacity: usize,
    ) -> Result<Self> {
        Self::create_internal(
            id.into(),
            queries,
            config,
            Some(priority_queue_capacity),
            true,
        )
        .await
    }

    /// Create from builder (internal method)
    pub(crate) async fn from_builder(
        id: String,
        queries: Vec<String>,
        config: MsSqlStoredProcReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Result<Self> {
        Self::create_internal(id, queries, config, priority_queue_capacity, auto_start).await
    }

    /// Internal constructor
    async fn create_internal(
        id: String,
        queries: Vec<String>,
        config: MsSqlStoredProcReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Result<Self> {
        // Validate configuration (including template compilation and route keys)
        config.validate(&queries)?;

        // Create reaction base
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }

        let base = ReactionBase::new(params);

        // If config has identity_provider, store it in base for unified access
        if let Some(ip) = &config.identity_provider {
            base.set_identity_provider(Arc::from(ip.clone_box())).await;
        }

        Ok(Self {
            base,
            config,
            executor: RwLock::new(None),
        })
    }
}

/// Process a single dequeued [`QueryResult`]: render and execute a command for
/// each diff item that has a configured template.
async fn process_query_result(
    reaction_id: &str,
    config: &MsSqlStoredProcReactionConfig,
    executor: &MsSqlExecutor,
    query_result: &QueryResult,
) {
    debug!(
        "[{reaction_id}] Processing {} result(s) from query: {}",
        query_result.results.len(),
        query_result.query_id
    );

    for diff in &query_result.results {
        // Noop items produce no context and no command.
        if matches!(diff, ResultDiff::Noop) {
            continue;
        }

        let Some((operation, context)) = build_context(query_result, diff) else {
            continue;
        };

        // Resolve the command template for this query and operation type.
        let Some(template) = config.get_command_template(&query_result.query_id, operation) else {
            debug!(
                "[{reaction_id}] No command configured for {} operation on query '{}', skipping",
                context
                    .get("operation")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?"),
                query_result.query_id
            );
            continue;
        };

        if template.is_empty() {
            continue;
        }

        // Render the command to SQL + ordered positional parameters.
        // A render failure (e.g. a referenced field is missing under strict
        // mode) skips this item rather than executing a command built from
        // missing data.
        let (command, params) = match render_command(&template, &context) {
            Ok(rendered) => rendered,
            Err(e) => {
                error!("[{reaction_id}] Failed to render command template: {e}");
                continue;
            }
        };

        debug!(
            "[{reaction_id}] Executing command with {} parameter(s): {command}",
            params.len()
        );

        match executor.execute_command(&command, params).await {
            Ok(()) => {
                debug!("[{reaction_id}] Successfully executed command");
            }
            Err(e) => {
                error!("[{reaction_id}] Failed to execute command: {e}");
            }
        }
    }
}

/// Run the processing loop until the shutdown signal fires or the queue closes.
async fn run_processing_loop(
    reaction_id: String,
    base: ReactionBase,
    config: MsSqlStoredProcReactionConfig,
    executor: Arc<MsSqlExecutor>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    info!("[{reaction_id}] Starting processing loop");

    loop {
        let query_result = tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                debug!("[{reaction_id}] Received shutdown signal, exiting processing loop");
                break;
            }
            result = base.priority_queue.dequeue() => result,
        };

        if query_result.results.is_empty() {
            continue;
        }

        process_query_result(&reaction_id, &config, &executor, query_result.as_ref()).await;
    }
}

#[async_trait]
impl Reaction for MsSqlStoredProcReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "storedproc-mssql"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        use crate::descriptor::{
            MsSqlStoredProcReactionConfigDto, StoredProcQueryConfigDto, StoredProcTemplateSpecDto,
        };
        use drasi_plugin_sdk::ConfigValue;

        fn map_spec_to_dto(spec: &crate::TemplateSpec) -> StoredProcTemplateSpecDto {
            StoredProcTemplateSpecDto {
                template: spec.template.clone(),
            }
        }

        fn map_qc_to_dto(qc: &crate::QueryConfig) -> StoredProcQueryConfigDto {
            StoredProcQueryConfigDto {
                added: qc.added.as_ref().map(map_spec_to_dto),
                updated: qc.updated.as_ref().map(map_spec_to_dto),
                deleted: qc.deleted.as_ref().map(map_spec_to_dto),
            }
        }

        let dto = MsSqlStoredProcReactionConfigDto {
            hostname: Some(ConfigValue::Static(self.config.hostname.clone())),
            port: self.config.port.map(ConfigValue::Static),
            user: ConfigValue::Static(self.config.user.clone()),
            password: ConfigValue::Static(self.config.password.clone()),
            database: ConfigValue::Static(self.config.database.clone()),
            ssl: Some(ConfigValue::Static(self.config.ssl)),
            routes: self
                .config
                .routes
                .iter()
                .map(|(k, v)| (k.clone(), map_qc_to_dto(v)))
                .collect(),
            default_template: self.config.default_template.as_ref().map(map_qc_to_dto),
            command_timeout_ms: Some(ConfigValue::Static(self.config.command_timeout_ms)),
            retry_attempts: Some(ConfigValue::Static(self.config.retry_attempts)),
        };

        self.base.properties_or_serialize(&dto)
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        log_component_start("MS SQL Server StoredProc Reaction", &self.base.id);

        info!(
            "[{}] Starting MS SQL Server StoredProc reaction for {}",
            self.base.id, self.config.database
        );

        // Create executor (deferred to start so identity_provider from context is available)
        let identity_provider = self.base.identity_provider().await;
        let executor = match MsSqlExecutor::new(&self.config, identity_provider).await {
            Ok(executor) => Arc::new(executor),
            Err(e) => {
                self.base
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!("Failed to connect to MS SQL Server: {e}")),
                    )
                    .await;
                return Err(e);
            }
        };

        // Test database connection
        if let Err(e) = executor.test_connection().await {
            self.base
                .set_status(
                    ComponentStatus::Error,
                    Some(format!("MS SQL connection test failed: {e}")),
                )
                .await;
            return Err(e);
        }

        // Store executor for later use
        *self.executor.write().await = Some(executor.clone());

        // Spawn the processing loop with a shutdown channel owned by the base,
        // so `stop_common` can interrupt it promptly.
        let shutdown_rx = self.base.create_shutdown_channel().await;
        let handle = tokio::spawn(run_processing_loop(
            self.base.id.clone(),
            self.base.clone_shared(),
            self.config.clone(),
            executor,
            shutdown_rx,
        ));
        self.base.set_processing_task(handle).await;

        self.base
            .set_status(
                ComponentStatus::Running,
                Some("MS SQL Server StoredProc reaction running".to_string()),
            )
            .await;

        info!(
            "[{}] MS SQL Server StoredProc reaction started successfully",
            self.base.id
        );
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!(
            "[{}] Stopping MS SQL Server StoredProc reaction",
            self.base.id
        );
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(
        &self,
        result: drasi_lib::channels::QueryResult,
    ) -> anyhow::Result<()> {
        self.base.enqueue_query_result(result).await
    }

    fn is_durable(&self) -> bool {
        false
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        false
    }

    fn default_recovery_policy(&self) -> drasi_lib::recovery::ReactionRecoveryPolicy {
        drasi_lib::recovery::ReactionRecoveryPolicy::Strict
    }
}

/// Builder for MsSqlStoredProcReaction
pub struct MsSqlStoredProcReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: MsSqlStoredProcReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl MsSqlStoredProcReactionBuilder {
    /// Create a new builder with the given reaction ID
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: MsSqlStoredProcReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    /// Set the database connection parameters
    pub fn with_connection(
        mut self,
        hostname: impl Into<String>,
        port: u16,
        database: impl Into<String>,
        user: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.config.hostname = hostname.into();
        self.config.port = Some(port);
        self.config.database = database.into();
        self.config.user = user.into();
        self.config.password = password.into();
        self
    }

    /// Set the database hostname
    pub fn with_hostname(mut self, hostname: impl Into<String>) -> Self {
        self.config.hostname = hostname.into();
        self
    }

    /// Set the database port
    pub fn with_port(mut self, port: u16) -> Self {
        self.config.port = Some(port);
        self
    }

    /// Set the database name
    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.config.database = database.into();
        self
    }

    /// Set the database user
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.config.user = user.into();
        self
    }

    /// Set the database password
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.config.password = password.into();
        self
    }

    /// Set the identity provider for authentication
    ///
    /// This takes precedence over `with_user` and `with_password`.
    /// Use this for cloud authentication (Azure Managed Identity, AWS IAM, etc.)
    pub fn with_identity_provider(
        mut self,
        provider: impl drasi_lib::identity::IdentityProvider + 'static,
    ) -> Self {
        self.config.identity_provider = Some(Box::new(provider));
        self
    }

    /// Enable or disable SSL/TLS
    pub fn with_ssl(mut self, enable: bool) -> Self {
        self.config.ssl = enable;
        self
    }

    /// Set the default template for all queries and operations
    pub fn with_default_template(mut self, template: QueryConfig) -> Self {
        self.config.default_template = Some(template);
        self
    }

    /// Set the command template for ADD operations in the default template.
    ///
    /// The command is a Handlebars template; use `{{param after.<field>}}` to
    /// bind row fields as positional parameters.
    pub fn with_added_command(mut self, command: impl Into<String>) -> Self {
        use crate::config::TemplateSpec;
        self.config
            .default_template
            .get_or_insert_with(QueryConfig::default)
            .added = Some(TemplateSpec::new(command));
        self
    }

    /// Set the command template for UPDATE operations in the default template.
    ///
    /// The command is a Handlebars template; use `{{param before.<field>}}` /
    /// `{{param after.<field>}}` to bind row fields as positional parameters.
    pub fn with_updated_command(mut self, command: impl Into<String>) -> Self {
        use crate::config::TemplateSpec;
        self.config
            .default_template
            .get_or_insert_with(QueryConfig::default)
            .updated = Some(TemplateSpec::new(command));
        self
    }

    /// Set the command template for DELETE operations in the default template.
    ///
    /// The command is a Handlebars template; use `{{param before.<field>}}` to
    /// bind row fields as positional parameters.
    pub fn with_deleted_command(mut self, command: impl Into<String>) -> Self {
        use crate::config::TemplateSpec;
        self.config
            .default_template
            .get_or_insert_with(QueryConfig::default)
            .deleted = Some(TemplateSpec::new(command));
        self
    }

    /// Add a route for a specific query ID
    pub fn with_route(mut self, query_id: impl Into<String>, config: QueryConfig) -> Self {
        self.config.routes.insert(query_id.into(), config);
        self
    }

    /// Add a query to subscribe to
    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Set all queries to subscribe to
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    /// Set the command timeout in milliseconds
    pub fn with_command_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.config.command_timeout_ms = timeout_ms;
        self
    }

    /// Set the number of retry attempts on failure
    pub fn with_retry_attempts(mut self, attempts: u32) -> Self {
        self.config.retry_attempts = attempts;
        self
    }

    /// Set the priority queue capacity
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set whether the reaction should auto-start
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set the full configuration at once
    pub fn with_config(mut self, config: MsSqlStoredProcReactionConfig) -> Self {
        self.config = config;
        self
    }

    /// Set the stored procedure name (shortcut for simple configurations)
    ///
    /// Creates default templates that execute the specified procedure, binding
    /// the changed row as a single JSON parameter:
    /// - added: passes the `after` row (as JSON)
    /// - updated: passes the `before` and `after` rows (as JSON)
    /// - deleted: passes the `before` row (as JSON)
    ///
    /// For per-field parameters, configure templates directly with
    /// [`Self::with_default_template`] / [`Self::with_route`] using
    /// `{{param after.<field>}}`.
    pub fn with_stored_procedure(mut self, proc_name: impl Into<String>) -> Self {
        use crate::config::TemplateSpec;

        let proc = proc_name.into();
        let query_config = QueryConfig {
            added: Some(TemplateSpec::new(format!(
                "EXEC {proc} {{{{param (json after)}}}}"
            ))),
            updated: Some(TemplateSpec::new(format!(
                "EXEC {proc} {{{{param (json before)}}}}, {{{{param (json after)}}}}"
            ))),
            deleted: Some(TemplateSpec::new(format!(
                "EXEC {proc} {{{{param (json before)}}}}"
            ))),
        };
        self.config.default_template = Some(query_config);
        self
    }

    /// Build the MsSqlStoredProcReaction
    pub async fn build(self) -> Result<MsSqlStoredProcReaction> {
        MsSqlStoredProcReaction::from_builder(
            self.id,
            self.queries,
            self.config,
            self.priority_queue_capacity,
            self.auto_start,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::{recovery::ReactionRecoveryPolicy, Reaction};

    #[tokio::test]
    async fn test_recovery_trait_defaults() {
        let reaction = MsSqlStoredProcReaction::builder("test-mssql")
            .with_connection("localhost", 1433, "testdb", "testuser", "testpass")
            .with_stored_procedure("test_proc")
            .build()
            .await
            .unwrap();

        assert!(!reaction.is_durable());
        assert!(!reaction.needs_snapshot_on_fresh_start());
        assert_eq!(
            reaction.default_recovery_policy(),
            ReactionRecoveryPolicy::Strict
        );
    }
}
