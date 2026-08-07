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

//! MySQL Stored Procedure reaction implementation.

use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use drasi_lib::channels::{ComponentStatus, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::Reaction;
use serde_json::{json, Value};

use crate::config::{MySqlStoredProcReactionConfig, QueryConfig};
use crate::executor::MySqlExecutor;
use crate::render::CommandRenderer;
use drasi_lib::reactions::common::OperationType;

/// MySQL Stored Procedure reaction
///
/// Invokes MySQL stored procedures when continuous query results change.
/// Supports different procedures for ADD, UPDATE, and DELETE operations.
pub struct MySqlStoredProcReaction {
    pub(crate) base: ReactionBase,
    config: MySqlStoredProcReactionConfig,
    executor: RwLock<Option<Arc<MySqlExecutor>>>,
}

impl std::fmt::Debug for MySqlStoredProcReaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MySqlStoredProcReaction")
            .field("config", &self.config)
            .finish()
    }
}

impl MySqlStoredProcReaction {
    /// Create a builder for MySqlStoredProcReaction
    pub fn builder(id: impl Into<String>) -> MySqlStoredProcReactionBuilder {
        MySqlStoredProcReactionBuilder::new(id)
    }

    /// Create a new MySQL stored procedure reaction
    pub async fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: MySqlStoredProcReactionConfig,
    ) -> Result<Self> {
        Self::create_internal(id.into(), queries, config, None, true).await
    }

    /// Create a new MySQL stored procedure reaction with custom priority queue capacity
    pub async fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: MySqlStoredProcReactionConfig,
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
        config: MySqlStoredProcReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Result<Self> {
        Self::create_internal(id, queries, config, priority_queue_capacity, auto_start).await
    }

    /// Internal constructor
    async fn create_internal(
        id: String,
        queries: Vec<String>,
        config: MySqlStoredProcReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Result<Self> {
        // Validate configuration
        config.validate()?;
        // Reject route keys that don't match a subscribed query.
        config.validate_routes(&queries)?;

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

    /// Build the template render context for a single diff item.
    ///
    /// Populates the standard context keys required by the Reaction Developer
    /// Guide §11: `query_id`/`query_name`, `operation`, `timestamp`,
    /// `metadata`, and the operation-appropriate `before`/`after`/`data`.
    fn build_context(
        query_id: &str,
        operation: OperationType,
        timestamp: &str,
        metadata: &HashMap<String, Value>,
        before: Option<&Value>,
        after: Option<&Value>,
        data: Option<&Value>,
    ) -> Value {
        let mut context = serde_json::Map::new();
        context.insert("query_id".to_string(), json!(query_id));
        context.insert("query_name".to_string(), json!(query_id));
        context.insert(
            "operation".to_string(),
            json!(operation.as_str().to_uppercase()),
        );
        context.insert("timestamp".to_string(), json!(timestamp));
        context.insert(
            "metadata".to_string(),
            json!(metadata
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<serde_json::Map<_, _>>()),
        );
        if let Some(before) = before {
            context.insert("before".to_string(), before.clone());
        }
        if let Some(after) = after {
            context.insert("after".to_string(), after.clone());
        }
        if let Some(data) = data {
            context.insert("data".to_string(), data.clone());
        }
        Value::Object(context)
    }

    /// Spawn the processing task that renders and executes commands.
    fn spawn_processing_task(
        &self,
        executor: Arc<MySqlExecutor>,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> tokio::task::JoinHandle<()> {
        let priority_queue = self.base.priority_queue.clone();
        let config = self.config.clone();
        let reaction_id = self.base.id.clone();

        tokio::spawn(async move {
            let renderer = CommandRenderer::new();
            info!("[{reaction_id}] Starting processing loop");

            loop {
                // Dequeue the next result, or break promptly on shutdown.
                let query_result_arc = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_id}] Shutdown signal received");
                        break;
                    }
                    result = priority_queue.dequeue() => result,
                };
                let query_result = query_result_arc.as_ref();
                let timestamp = query_result.timestamp.to_rfc3339();

                debug!(
                    "[{}] Processing result from query: {}",
                    reaction_id, query_result.query_id
                );

                // Process each result item in the batch (empty batch = no-op).
                for result_item in &query_result.results {
                    // Map the diff variant to (operation, context fields).
                    let (operation, result_type, context) = match result_item {
                        ResultDiff::Add { data, .. } => (
                            OperationType::Add,
                            "ADD",
                            Self::build_context(
                                &query_result.query_id,
                                OperationType::Add,
                                &timestamp,
                                &query_result.metadata,
                                None,
                                Some(data),
                                None,
                            ),
                        ),
                        ResultDiff::Update {
                            data,
                            before,
                            after,
                            ..
                        } => (
                            OperationType::Update,
                            "UPDATE",
                            Self::build_context(
                                &query_result.query_id,
                                OperationType::Update,
                                &timestamp,
                                &query_result.metadata,
                                Some(before),
                                Some(after),
                                Some(data),
                            ),
                        ),
                        ResultDiff::Delete { data, .. } => (
                            OperationType::Delete,
                            "DELETE",
                            Self::build_context(
                                &query_result.query_id,
                                OperationType::Delete,
                                &timestamp,
                                &query_result.metadata,
                                Some(data),
                                None,
                                None,
                            ),
                        ),
                        ResultDiff::Aggregation { before, after, .. } => (
                            OperationType::Update,
                            "AGGREGATION",
                            Self::build_context(
                                &query_result.query_id,
                                OperationType::Update,
                                &timestamp,
                                &query_result.metadata,
                                before.as_ref(),
                                Some(after),
                                None,
                            ),
                        ),
                        ResultDiff::Noop => continue,
                    };

                    // Resolve the command template for this query/operation.
                    let Some(template) =
                        config.resolve_command_template(&query_result.query_id, operation)
                    else {
                        debug!(
                            "[{reaction_id}] No command configured for {result_type} operation, skipping"
                        );
                        continue;
                    };

                    // Render the command with positional parameter binding.
                    let rendered = match renderer.render(&template, &context) {
                        Ok(rendered) => rendered,
                        Err(e) => {
                            warn!(
                                "[{reaction_id}] Failed to render command for {result_type} operation: {e}; skipping event"
                            );
                            continue;
                        }
                    };

                    debug!(
                        "[{reaction_id}] Executing '{}' with {} parameters for {result_type} operation",
                        rendered.sql,
                        rendered.params.len()
                    );

                    match executor
                        .execute_command(&rendered.sql, rendered.params)
                        .await
                    {
                        Ok(()) => {
                            debug!(
                                "[{reaction_id}] Successfully executed command for {result_type} operation"
                            );
                        }
                        Err(e) => {
                            error!("[{reaction_id}] Failed to execute command: {e}");
                        }
                    }
                }
            }

            info!("[{reaction_id}] Processing loop stopped");
        })
    }
}

#[async_trait]
impl Reaction for MySqlStoredProcReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "storedproc-mysql"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        use crate::descriptor::{
            MySqlStoredProcReactionConfigDto, StoredProcQueryConfigDto, StoredProcTemplateSpecDto,
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

        let dto = MySqlStoredProcReactionConfigDto {
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

    async fn initialize(&self, context: drasi_lib::context::ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        log_component_start("MySQL StoredProc Reaction", &self.base.id);
        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting MySQL StoredProc reaction".to_string()),
            )
            .await;

        info!(
            "[{}] Starting MySQL StoredProc reaction for {}",
            self.base.id, self.config.database
        );

        // Create executor (deferred to start so identity_provider from context is available)
        let identity_provider = self.base.identity_provider().await;
        let executor = Arc::new(MySqlExecutor::new(&self.config, identity_provider).await?);

        // Test database connection
        executor.test_connection().await?;

        // Store executor for later use
        *self.executor.write().await = Some(executor.clone());

        self.base
            .set_status(
                ComponentStatus::Running,
                Some("MySQL StoredProc reaction started".to_string()),
            )
            .await;

        // Spawn processing task with a shutdown channel so stop_common can
        // interrupt the loop promptly.
        let shutdown_rx = self.base.create_shutdown_channel().await;
        let task = self.spawn_processing_task(executor, shutdown_rx);
        self.base.set_processing_task(task).await;

        info!(
            "[{}] MySQL StoredProc reaction started successfully",
            self.base.id
        );
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("[{}] Stopping MySQL StoredProc reaction", self.base.id);

        self.base.stop_common().await?;
        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("MySQL StoredProc reaction stopped".to_string()),
            )
            .await;

        info!("[{}] MySQL StoredProc reaction stopped", self.base.id);
        Ok(())
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

/// Builder for MySqlStoredProcReaction
pub struct MySqlStoredProcReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: MySqlStoredProcReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl MySqlStoredProcReactionBuilder {
    /// Create a new builder with the given reaction ID
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: MySqlStoredProcReactionConfig::default(),
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
    ///
    /// SSL certificates must be installed in the system trust store.
    /// For AWS RDS on macOS:
    ///   sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain ~/rds-ca-bundle.pem
    pub fn with_ssl(mut self, enable: bool) -> Self {
        self.config.ssl = enable;
        self
    }

    /// Set the default template configuration
    pub fn with_default_template(mut self, template: QueryConfig) -> Self {
        self.config.default_template = Some(template);
        self
    }

    /// Add a query-specific template route
    pub fn with_route(mut self, query_id: impl Into<String>, template: QueryConfig) -> Self {
        self.config.routes.insert(query_id.into(), template);
        self
    }

    /// Add a query to subscribe to
    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Add a query to subscribe to (alias for [`with_query`](Self::with_query)).
    pub fn from_query(self, query_id: impl Into<String>) -> Self {
        self.with_query(query_id)
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
    pub fn with_config(mut self, config: MySqlStoredProcReactionConfig) -> Self {
        self.config = config;
        self
    }

    /// Set the stored procedure name (shortcut for simple configurations)
    ///
    /// Creates default templates that call the specified procedure with:
    /// - added: binds the whole `after` row
    /// - updated: binds the whole `before` and `after` rows
    /// - deleted: binds the whole `before` row
    pub fn with_stored_procedure(mut self, proc_name: impl Into<String>) -> Self {
        use crate::config::TemplateSpec;

        let proc = proc_name.into();
        let query_config = QueryConfig {
            added: Some(TemplateSpec::new(format!(
                "CALL {proc}({{{{param after}}}})"
            ))),
            updated: Some(TemplateSpec::new(format!(
                "CALL {proc}({{{{param before}}}}, {{{{param after}}}})"
            ))),
            deleted: Some(TemplateSpec::new(format!(
                "CALL {proc}({{{{param before}}}})"
            ))),
        };
        self.config.default_template = Some(query_config);
        self
    }

    /// Build the MySqlStoredProcReaction
    pub async fn build(self) -> Result<MySqlStoredProcReaction> {
        MySqlStoredProcReaction::from_builder(
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
        let reaction = MySqlStoredProcReaction::builder("test-mysql")
            .with_connection("localhost", 3306, "testdb", "testuser", "testpass")
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
