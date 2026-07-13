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
use tokio::sync::RwLock;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::reactions::common::TemplateRouting;
use drasi_lib::Reaction;

use crate::config::{PostgresStoredProcReactionConfig, QueryConfig};
use crate::executor::PostgresExecutor;
use crate::render;

/// PostgreSQL Stored Procedure reaction
///
/// Invokes PostgreSQL stored procedures when continuous query results change.
/// Supports different procedures for ADD, UPDATE, and DELETE operations.
pub struct PostgresStoredProcReaction {
    pub(crate) base: ReactionBase,
    config: PostgresStoredProcReactionConfig,
    templates: Arc<HashMap<String, render::CompiledTemplate>>,
    executor: RwLock<Option<Arc<PostgresExecutor>>>,
}

impl std::fmt::Debug for PostgresStoredProcReaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresStoredProcReaction")
            .field("config", &self.config)
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
        config.validate(&queries)?;

        // Pre-compile all templates once so the render path never re-parses.
        let templates = Arc::new(config.compile_templates()?);

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
            templates,
            executor: RwLock::new(None),
        })
    }

    /// Test the database connection
    pub async fn test_connection(&self) -> Result<()> {
        let guard = self.executor.read().await;
        match guard.as_ref() {
            Some(executor) => executor.test_connection().await,
            None => Err(anyhow::anyhow!(
                "Executor not initialized — call start() first"
            )),
        }
    }

    /// Run the shutdown-aware processing loop until the runtime signals
    /// shutdown or the queue closes.
    async fn run_processing_loop(
        base: ReactionBase,
        config: PostgresStoredProcReactionConfig,
        templates: Arc<HashMap<String, render::CompiledTemplate>>,
        executor: Arc<PostgresExecutor>,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) {
        let reaction_id = base.id.clone();
        info!("[{reaction_id}] Starting processing loop");

        loop {
            let query_result_arc = tokio::select! {
                biased;
                _ = &mut shutdown_rx => {
                    debug!("[{reaction_id}] Received shutdown signal, exiting processing loop");
                    break;
                }
                result = base.priority_queue.dequeue() => result,
            };
            let query_result = query_result_arc.as_ref();

            // Empty result sets are no-ops.
            if query_result.results.is_empty() {
                continue;
            }

            debug!(
                "[{reaction_id}] Processing {} results from query: {}",
                query_result.results.len(),
                query_result.query_id
            );

            for result_item in &query_result.results {
                // Noop variants carry no change; skip them.
                let render_input = match render::build_render_input(query_result, result_item) {
                    Some(input) => input,
                    None => continue,
                };

                // Resolve the command template following the guide's route
                // resolution order. No template configured -> nothing to run.
                let spec = match config
                    .get_template_spec(&query_result.query_id, render_input.operation)
                {
                    Some(spec) if !spec.template.is_empty() => spec,
                    _ => {
                        debug!(
                                "[{reaction_id}] No command configured for {:?} operation on '{}', skipping",
                                render_input.operation, query_result.query_id
                            );
                        continue;
                    }
                };

                // Render the command to SQL + positional bind parameters using
                // the pre-compiled template. On a render failure (e.g. a missing
                // referenced field) we log and skip the event rather than
                // executing partial or unsafe SQL.
                let compiled = match templates.get(&spec.template) {
                    Some(compiled) => compiled,
                    None => {
                        // Every configured template is compiled at construction,
                        // so this should not happen; skip defensively.
                        error!(
                            "[{reaction_id}] No compiled template found for query '{}', skipping",
                            query_result.query_id
                        );
                        continue;
                    }
                };
                let (command, params) = match compiled.render(&render_input.context) {
                    Ok(rendered) => rendered,
                    Err(e) => {
                        error!(
                            "[{reaction_id}] Failed to render command for query '{}': {e}",
                            query_result.query_id
                        );
                        continue;
                    }
                };

                debug!(
                    "[{reaction_id}] Executing command with {} parameters for {:?} operation",
                    params.len(),
                    render_input.operation
                );

                if let Err(e) = executor.execute_command(&command, params).await {
                    error!("[{reaction_id}] Failed to execute command: {e}");
                }
            }
        }

        info!("[{reaction_id}] PostgreSQL StoredProc processing loop stopped");
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
        use crate::descriptor::{
            PostgresStoredProcReactionConfigDto, StoredProcQueryConfigDto,
            StoredProcTemplateSpecDto,
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

        let dto = PostgresStoredProcReactionConfigDto {
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
        log_component_start("PostgreSQL StoredProc Reaction", &self.base.id);

        info!(
            "[{}] Starting PostgreSQL StoredProc reaction for {}",
            self.base.id, self.config.database
        );

        self.base.set_status(ComponentStatus::Starting, None).await;

        // Create executor (deferred to start so identity_provider from context is available)
        let identity_provider = self.base.identity_provider().await;
        let executor = Arc::new(PostgresExecutor::new(&self.config, identity_provider).await?);

        // Test database connection
        executor.test_connection().await?;

        // Store executor for later use (e.g. test_connection)
        *self.executor.write().await = Some(executor.clone());

        // Create the shutdown channel and spawn the processing loop.
        let shutdown_rx = self.base.create_shutdown_channel().await;
        let base = self.base.clone_shared();
        let config = self.config.clone();
        let templates = Arc::clone(&self.templates);
        let handle = tokio::spawn(async move {
            Self::run_processing_loop(base, config, templates, executor, shutdown_rx).await;
        });
        self.base.set_processing_task(handle).await;

        self.base.set_status(ComponentStatus::Running, None).await;

        info!(
            "[{}] PostgreSQL StoredProc reaction started successfully",
            self.base.id
        );
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("[{}] Stopping PostgreSQL StoredProc reaction", self.base.id);
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

    /// Alias of [`with_query`](Self::with_query) for readability at call sites.
    pub fn from_query(mut self, query_id: impl Into<String>) -> Self {
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

    /// Configure a procedure that accepts the changed row as a single JSONB
    /// argument (shortcut for the common "sync the whole row" case).
    ///
    /// This creates default templates that call `proc_name` with the whole row
    /// object bound as a single positional JSONB parameter:
    /// - added: `CALL proc_name({{param after}})`
    /// - updated: `CALL proc_name({{param before}}, {{param after}})`
    /// - deleted: `CALL proc_name({{param before}})`
    ///
    /// # Important
    ///
    /// The procedure **must** accept a single `jsonb` argument (two for the
    /// update case: `before` then `after`). It is *not* suitable for procedures
    /// that take individual typed columns such as `(id INT, name TEXT)` — those
    /// would fail with a type mismatch. For typed-argument procedures, configure
    /// explicit templates via [`with_default_template`](Self::with_default_template)
    /// or [`with_route`](Self::with_route) that reference each field, for example
    /// `CALL add_user({{param after.id}}, {{param after.name}})`.
    pub fn with_jsonb_procedure(mut self, proc_name: impl Into<String>) -> Self {
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

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::{recovery::ReactionRecoveryPolicy, Reaction};

    #[tokio::test]
    async fn test_recovery_trait_defaults() {
        let reaction = PostgresStoredProcReaction::builder("test-postgres")
            .with_connection("localhost", 5432, "testdb", "testuser", "testpass")
            .with_jsonb_procedure("test_proc")
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
