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

//! PostgreSQL Stored Procedure reaction implementation.

use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use drasi_lib::channels::{ComponentStatus, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::Reaction;

use crate::config::{PostgresStoredProcReactionConfig, QueryConfig};
use crate::executor::PostgresExecutor;
use crate::parser::ParameterParser;
use drasi_lib::reactions::common::OperationType;

/// PostgreSQL Stored Procedure reaction
///
/// Invokes PostgreSQL stored procedures when continuous query results change.
/// Supports different procedures for ADD, UPDATE, and DELETE operations.
pub struct PostgresStoredProcReaction {
    base: ReactionBase,
    config: PostgresStoredProcReactionConfig,
    executor: Arc<PostgresExecutor>,
    parser: ParameterParser,
    task_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl std::fmt::Debug for PostgresStoredProcReaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresStoredProcReaction")
            .field("config", &self.config)
            .field("parser", &self.parser)
            .finish()
    }
}

impl PostgresStoredProcReaction {
    /// Create a builder for PostgresStoredProcReaction
    pub fn builder(id: impl Into<String>) -> PostgresStoredProcReactionBuilder {
        PostgresStoredProcReactionBuilder::new(id)
    }

    /// Create a new PostgreSQL stored procedure reaction
    pub async fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: PostgresStoredProcReactionConfig,
    ) -> Result<Self> {
        Self::create_internal(id.into(), queries, config, None, true).await
    }

    /// Create a new PostgreSQL stored procedure reaction with custom priority queue capacity
    pub async fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: PostgresStoredProcReactionConfig,
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
        config: PostgresStoredProcReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Result<Self> {
        Self::create_internal(id, queries, config, priority_queue_capacity, auto_start).await
    }

    /// Internal constructor
    async fn create_internal(
        id: String,
        queries: Vec<String>,
        config: PostgresStoredProcReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Result<Self> {
        // Validate configuration
        config.validate()?;

        // Create database executor
        let executor = Arc::new(PostgresExecutor::new(&config).await?);

        // Create reaction base
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }

        Ok(Self {
            base: ReactionBase::new(params),
            config,
            executor,
            parser: ParameterParser::new(),
            task_handle: Arc::new(Mutex::new(None)),
        })
    }

    /// Test the database connection
    pub async fn test_connection(&self) -> Result<()> {
        self.executor.test_connection().await
    }

    /// Prepare context data with after/before fields based on operation type
    fn prepare_context(operation: OperationType, data: &serde_json::Value) -> serde_json::Value {
        use serde_json::json;

        match operation {
            OperationType::Add => {
                // For ADD, data goes into "after"
                json!({
                    "after": data.clone()
                })
            }
            OperationType::Update => {
                // For UPDATE, check if data already has before/after fields
                if let Some(obj) = data.as_object() {
                    let mut context = serde_json::Map::new();
                    if let Some(before) = obj.get("before") {
                        context.insert("before".to_string(), before.clone());
                    }
                    if let Some(after) = obj.get("after") {
                        context.insert("after".to_string(), after.clone());
                    }
                    // If no before/after, treat entire data as after
                    if context.is_empty() {
                        context.insert("after".to_string(), data.clone());
                    }
                    json!(context)
                } else {
                    json!({
                        "after": data.clone()
                    })
                }
            }
            OperationType::Delete => {
                // For DELETE, data goes into "before"
                json!({
                    "before": data.clone()
                })
            }
        }
    }

    /// Spawn the processing task that handles query results
    fn spawn_processing_task(&self) -> JoinHandle<()> {
        let priority_queue = self.base.priority_queue.clone();
        let executor = self.executor.clone();
        let parser = self.parser.clone();
        let config = self.config.clone();
        let reaction_id = self.base.id.clone();

        tokio::spawn(async move {
            info!("[{reaction_id}] Starting processing loop");
            loop {
                // Dequeue next result (blocks until available)
                let query_result_arc = priority_queue.dequeue().await;
                let query_result = (*query_result_arc).clone();

                debug!(
                    "[{}] Processing result from query: {}",
                    reaction_id, query_result.query_id
                );

                // Process each result item in the batch
                for result_item in &query_result.results {
                    let (operation, data_value, result_type) = match result_item {
                        ResultDiff::Add { data } => (OperationType::Add, data, "ADD"),
                        ResultDiff::Update { data, .. } => (OperationType::Update, data, "UPDATE"),
                        ResultDiff::Delete { data } => (OperationType::Delete, data, "DELETE"),
                        ResultDiff::Aggregation { .. } | ResultDiff::Noop => {
                            debug!(
                                "[{reaction_id}] Unknown operation type: aggregation/noop, skipping"
                            );
                            continue;
                        }
                    };

                    // Get the command template for this query and operation type
                    let command = config.get_command_template(&query_result.query_id, operation);

                    // Execute the stored procedure if a command is configured
                    if let Some(cmd) = command {
                        // Prepare context with after/before fields based on operation type
                        let context = Self::prepare_context(operation, data_value);
                        debug!(
                            "[{reaction_id}] Context data: {}",
                            serde_json::to_string_pretty(&context)
                                .unwrap_or_else(|_| "<<invalid>>".to_string())
                        );

                        // Parse command and extract parameters
                        match parser.parse_command(&cmd, &context) {
                            Ok((proc_name, params)) => {
                                debug!(
                                    "[{reaction_id}] Executing procedure: {proc_name} with {params_len} parameters for {result_type} operation",
                                    params_len = params.len()
                                );

                                // Execute stored procedure
                                match executor.execute_procedure(&proc_name, params).await {
                                    Ok(()) => {
                                        debug!(
                                            "[{reaction_id}] Successfully executed {proc_name} for {result_type} operation"
                                        );
                                    }
                                    Err(e) => {
                                        error!(
                                            "[{reaction_id}] Failed to execute {proc_name}: {e}"
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                error!("[{reaction_id}] Failed to parse command: {e}");
                            }
                        }
                    } else {
                        debug!(
                            "[{reaction_id}] No command configured for {result_type} operation, skipping"
                        );
                    }
                }
            }
        })
    }
}

#[async_trait]
impl Reaction for PostgresStoredProcReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "storedproc-postgres"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert("database".to_string(), serde_json::json!("PostgreSQL"));
        props.insert(
            "hostname".to_string(),
            serde_json::json!(self.config.hostname),
        );
        props.insert(
            "database_name".to_string(),
            serde_json::json!(self.config.database),
        );
        props.insert("ssl".to_string(), serde_json::json!(self.config.ssl));
        props
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
        log_component_start("PostgreSQL StoredProc Reaction", &self.base.id);

        info!(
            "[{}] Starting PostgreSQL StoredProc reaction for {}",
            self.base.id, self.config.database
        );

        // Test database connection
        self.executor.test_connection().await?;

        // Subscribe to all queries
        self.base.subscribe_to_queries().await?;

        // Spawn processing task
        let task = self.spawn_processing_task();
        *self.task_handle.lock().await = Some(task);

        info!(
            "[{}] PostgreSQL StoredProc reaction started successfully",
            self.base.id
        );
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("[{}] Stopping PostgreSQL StoredProc reaction", self.base.id);

        // Abort the processing task
        if let Some(handle) = self.task_handle.lock().await.take() {
            handle.abort();
        }

        info!("[{}] PostgreSQL StoredProc reaction stopped", self.base.id);
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
}

/// Builder for PostgresStoredProcReaction
pub struct PostgresStoredProcReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: PostgresStoredProcReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl PostgresStoredProcReactionBuilder {
    /// Create a new builder with the given reaction ID
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: PostgresStoredProcReactionConfig::default(),
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
    pub fn with_config(mut self, config: PostgresStoredProcReactionConfig) -> Self {
        self.config = config;
        self
    }

    /// Set the stored procedure name (shortcut for simple configurations)
    ///
    /// Creates default templates that call the specified procedure with:
    /// - added: passes @after data
    /// - updated: passes @before and @after data
    /// - deleted: passes @before data
    pub fn with_stored_procedure(mut self, proc_name: impl Into<String>) -> Self {
        use crate::config::TemplateSpec;

        let proc = proc_name.into();
        let query_config = QueryConfig {
            added: Some(TemplateSpec::new(format!("CALL {proc}(@after)"))),
            updated: Some(TemplateSpec::new(format!("CALL {proc}(@before, @after)"))),
            deleted: Some(TemplateSpec::new(format!("CALL {proc}(@before)"))),
        };
        self.config.default_template = Some(query_config);
        self
    }

    /// Build the PostgresStoredProcReaction
    pub async fn build(self) -> Result<PostgresStoredProcReaction> {
        PostgresStoredProcReaction::from_builder(
            self.id,
            self.queries,
            self.config,
            self.priority_queue_capacity,
            self.auto_start,
        )
        .await
    }
}
