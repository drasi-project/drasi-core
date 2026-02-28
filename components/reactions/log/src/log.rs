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

use super::config::LogReactionConfig;
use anyhow::Result;
use async_trait::async_trait;
use handlebars::Handlebars;
use log::debug;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

use drasi_lib::channels::{ComponentEventSender, ComponentStatus, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::{QueryProvider, Reaction};

pub struct LogReaction {
    base: ReactionBase,
    config: LogReactionConfig,
}

impl LogReaction {
    /// Create a new log reaction
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for the reaction
    /// * `queries` - Query IDs to subscribe to
    /// * `config` - Configuration including templates and routes
    ///
    /// # Returns
    ///
    /// Returns `Ok(LogReaction)` if all templates are valid and all routes correspond to subscribed queries.
    /// Returns `Err` if template validation fails or if a route doesn't match a subscribed query.
    ///
    /// # Errors
    ///
    /// - Returns error if any template has invalid Handlebars syntax
    /// - Returns error if a route query ID doesn't match any subscribed query
    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: LogReactionConfig,
    ) -> anyhow::Result<Self> {
        let id = id.into();

        // Validate templates and routes
        Self::validate_config(&queries, &config)?;

        let params = ReactionBaseParams::new(id, queries);
        Ok(Self {
            base: ReactionBase::new(params),
            config,
        })
    }

    /// Create a new log reaction with custom priority queue capacity
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for the reaction
    /// * `queries` - Query IDs to subscribe to
    /// * `config` - Configuration including templates and routes
    /// * `priority_queue_capacity` - Maximum events in priority queue
    ///
    /// # Returns
    ///
    /// Returns `Ok(LogReaction)` if all templates are valid and all routes correspond to subscribed queries.
    /// Returns `Err` if template validation fails or if a route doesn't match a subscribed query.
    ///
    /// # Errors
    ///
    /// - Returns error if any template has invalid Handlebars syntax
    /// - Returns error if a route query ID doesn't match any subscribed query
    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: LogReactionConfig,
        priority_queue_capacity: usize,
    ) -> anyhow::Result<Self> {
        let id = id.into();

        // Validate templates and routes
        Self::validate_config(&queries, &config)?;

        let params = ReactionBaseParams::new(id, queries)
            .with_priority_queue_capacity(priority_queue_capacity);
        Ok(Self {
            base: ReactionBase::new(params),
            config,
        })
    }

    /// Validate a template by attempting to compile it with Handlebars
    fn validate_template(template: &str) -> anyhow::Result<()> {
        if template.is_empty() {
            return Ok(());
        }
        // Compile the template to validate syntax without requiring data
        handlebars::Template::compile(template)
            .map_err(|e| anyhow::anyhow!("Invalid template: {e}"))?;
        Ok(())
    }

    /// Validate all templates in a QueryConfig
    fn validate_query_config(config: &crate::config::QueryConfig) -> anyhow::Result<()> {
        if let Some(added) = &config.added {
            Self::validate_template(&added.template)?;
        }
        if let Some(updated) = &config.updated {
            Self::validate_template(&updated.template)?;
        }
        if let Some(deleted) = &config.deleted {
            Self::validate_template(&deleted.template)?;
        }
        Ok(())
    }

    /// Validate configuration: templates and route-query matching
    fn validate_config(queries: &[String], config: &LogReactionConfig) -> anyhow::Result<()> {
        // Validate all templates in routes
        for (query_id, route_config) in &config.routes {
            Self::validate_query_config(route_config)
                .map_err(|e| anyhow::anyhow!("Invalid template in route '{query_id}': {e}"))?;
        }

        // Validate default template if provided
        if let Some(default_template) = &config.default_template {
            Self::validate_query_config(default_template)
                .map_err(|e| anyhow::anyhow!("Invalid default template: {e}"))?;
        }

        // Validate that all routes correspond to subscribed queries
        if !config.routes.is_empty() && !queries.is_empty() {
            for route_query in config.routes.keys() {
                // Pre-compute dotted format once per route
                let dotted_route = format!(".{route_query}");
                let matches = queries
                    .iter()
                    .any(|q| q == route_query || q.ends_with(&dotted_route));
                if !matches {
                    return Err(anyhow::anyhow!(
                        "Route '{route_query}' does not match any subscribed query. Subscribed queries: {queries:?}"
                    ));
                }
            }
        }

        Ok(())
    }

    #[allow(clippy::print_stdout)]
    fn log_result(&self, message: &str) {
        println!("[{}] {}", self.base.id, message);
    }

    /// Create a builder for LogReaction
    pub fn builder(id: impl Into<String>) -> LogReactionBuilder {
        LogReactionBuilder::new(id)
    }
}

/// Builder for LogReaction
pub struct LogReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: LogReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl LogReactionBuilder {
    /// Create a new builder with the given reaction ID
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: LogReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    /// Set the query IDs to subscribe to
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    /// Add a query ID to subscribe to
    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Connect this reaction to receive results from a query (alias for with_query)
    pub fn from_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Set custom priority queue capacity
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set whether the reaction should auto-start
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set default template configuration for all queries.
    ///
    /// The default template is used when no query-specific route is defined.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use drasi_reaction_log::{QueryConfig, TemplateSpec};
    ///
    /// let default_template = QueryConfig {
    ///     added: Some(TemplateSpec::new("[ADD] {{after.id}}")),
    ///     updated: Some(TemplateSpec::new("[UPD] {{after.id}}")),
    ///     deleted: Some(TemplateSpec::new("[DEL] {{before.id}}")),
    /// };
    ///
    /// let reaction = LogReaction::builder("my-logger")
    ///     .with_query("my-query")
    ///     .with_default_template(default_template)
    ///     .build()?;
    /// ```
    pub fn with_default_template(mut self, template: crate::config::QueryConfig) -> Self {
        self.config.default_template = Some(template);
        self
    }

    /// Set template configuration for a specific query.
    ///
    /// Query-specific templates override the default template.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use drasi_reaction_log::{QueryConfig, TemplateSpec};
    ///
    /// let sensor_config = QueryConfig {
    ///     added: Some(TemplateSpec {
    ///         template: "[SENSOR] {{after.id}}: {{after.temperature}}°C".to_string(),
    ///     }),
    ///     updated: None,
    ///     deleted: None,
    /// };
    ///
    /// let reaction = LogReaction::builder("my-logger")
    ///     .with_query("sensor-data")
    ///     .with_route("sensor-data", sensor_config)
    ///     .build();
    /// ```
    pub fn with_route(
        mut self,
        query_id: impl Into<String>,
        config: crate::config::QueryConfig,
    ) -> Self {
        self.config.routes.insert(query_id.into(), config);
        self
    }

    /// Build the LogReaction
    ///
    /// # Returns
    ///
    /// Returns `Ok(LogReaction)` if all templates are valid and all routes correspond to subscribed queries.
    /// Returns `Err` if template validation fails or if a route doesn't match a subscribed query.
    ///
    /// # Errors
    ///
    /// - Returns error if any template has invalid Handlebars syntax
    /// - Returns error if a route query ID doesn't match any subscribed query
    pub fn build(self) -> anyhow::Result<LogReaction> {
        // Validate templates and routes
        LogReaction::validate_config(&self.queries, &self.config)?;

        let mut params =
            ReactionBaseParams::new(self.id, self.queries).with_auto_start(self.auto_start);
        if let Some(capacity) = self.priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        Ok(LogReaction {
            base: ReactionBase::new(params),
            config: self.config,
        })
    }
}

#[async_trait]
impl Reaction for LogReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "log"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
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
        log_component_start("Reaction", &self.base.id);

        // Transition to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting log reaction".to_string()),
            )
            .await?;

        // Subscribe to all configured queries using ReactionBase
        // QueryProvider is available from initialize() context
        self.base.subscribe_to_queries().await?;

        // Transition to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("Log reaction started".to_string()),
            )
            .await?;

        self.log_result(&format!(
            "Started - receiving results from queries: {:?}",
            self.base.queries
        ));

        // Spawn processing task to dequeue and process results in timestamp order
        let priority_queue = self.base.priority_queue.clone();
        let reaction_name = self.base.id.clone();
        let config = self.config.clone();

        // Create shutdown channel for graceful termination
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        let processing_task = tokio::spawn(async move {
            // Set up Handlebars with json helper
            let mut handlebars = Handlebars::new();
            handlebars.register_helper(
                "json",
                Box::new(
                    |h: &handlebars::Helper,
                     _: &Handlebars,
                     _: &handlebars::Context,
                     _: &mut handlebars::RenderContext,
                     out: &mut dyn handlebars::Output|
                     -> handlebars::HelperResult {
                        if let Some(value) = h.param(0) {
                            let json_str = serde_json::to_string(&value.value())
                                .unwrap_or_else(|_| "null".to_string());
                            out.write(&json_str)?;
                        }
                        Ok(())
                    },
                ),
            );

            loop {
                // Use select to wait for either a result OR shutdown signal
                let query_result_arc = tokio::select! {
                    biased;

                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_name}] Received shutdown signal, exiting processing loop");
                        break;
                    }

                    result = priority_queue.dequeue() => result,
                };

                // Get mutable access to the result for profiling
                // Note: We need to clone and modify since Arc doesn't allow mutation
                let mut query_result = (*query_result_arc).clone();

                // Capture reaction_receive_ns timestamp
                if let Some(ref mut profiling) = query_result.profiling {
                    profiling.reaction_receive_ns = Some(drasi_lib::profiling::timestamp_ns());
                }

                if query_result.results.is_empty() {
                    debug!("[{reaction_name}] Received empty result set from query");
                    continue;
                }

                #[allow(clippy::print_stdout)]
                {
                    println!(
                        "[{}] Query '{}' ({} items):",
                        reaction_name,
                        query_result.query_id,
                        query_result.results.len()
                    );
                }

                for result in &query_result.results {
                    // Build context for template rendering
                    let mut context = Map::new();
                    context.insert(
                        "query_name".to_string(),
                        Value::String(query_result.query_id.clone()),
                    );

                    // Get query-specific config if available, otherwise use default
                    let query_config = config
                        .routes
                        .get(&query_result.query_id)
                        .or(config.default_template.as_ref());

                    match result {
                        ResultDiff::Add { data } => {
                            context.insert("operation".to_string(), Value::String("ADD".into()));
                            context.insert("after".to_string(), data.clone());

                            let template = query_config
                                .and_then(|qc| qc.added.as_ref())
                                .map(|ts| ts.template.as_str());

                            if let Some(template_str) = template {
                                match handlebars.render_template(template_str, &context) {
                                    Ok(rendered) => {
                                        #[allow(clippy::print_stdout)]
                                        {
                                            println!("[{reaction_name}]   {rendered}");
                                        }
                                    }
                                    Err(e) => {
                                        debug!("[{reaction_name}] Template render error: {e}");
                                        #[allow(clippy::print_stdout)]
                                        {
                                            println!("[{reaction_name}]   [ADD] {data}");
                                        }
                                    }
                                }
                            } else {
                                #[allow(clippy::print_stdout)]
                                {
                                    println!("[{reaction_name}]   [ADD] {data}");
                                }
                            }
                        }
                        ResultDiff::Delete { data } => {
                            context.insert("operation".to_string(), Value::String("DELETE".into()));
                            context.insert("before".to_string(), data.clone());

                            let template = query_config
                                .and_then(|qc| qc.deleted.as_ref())
                                .map(|ts| ts.template.as_str());

                            if let Some(template_str) = template {
                                match handlebars.render_template(template_str, &context) {
                                    Ok(rendered) => {
                                        #[allow(clippy::print_stdout)]
                                        {
                                            println!("[{reaction_name}]   {rendered}");
                                        }
                                    }
                                    Err(e) => {
                                        debug!("[{reaction_name}] Template render error: {e}");
                                        #[allow(clippy::print_stdout)]
                                        {
                                            println!("[{reaction_name}]   [DELETE] {data}");
                                        }
                                    }
                                }
                            } else {
                                #[allow(clippy::print_stdout)]
                                {
                                    println!("[{reaction_name}]   [DELETE] {data}");
                                }
                            }
                        }
                        ResultDiff::Update {
                            before,
                            after,
                            data,
                            ..
                        } => {
                            context.insert("operation".to_string(), Value::String("UPDATE".into()));
                            context.insert("before".to_string(), before.clone());
                            context.insert("after".to_string(), after.clone());
                            context.insert("data".to_string(), data.clone());

                            let template = query_config
                                .and_then(|qc| qc.updated.as_ref())
                                .map(|ts| ts.template.as_str());

                            if let Some(template_str) = template {
                                match handlebars.render_template(template_str, &context) {
                                    Ok(rendered) => {
                                        #[allow(clippy::print_stdout)]
                                        {
                                            println!("[{reaction_name}]   {rendered}");
                                        }
                                    }
                                    Err(e) => {
                                        debug!("[{reaction_name}] Template render error: {e}");
                                        #[allow(clippy::print_stdout)]
                                        {
                                            println!(
                                                "[{reaction_name}]   [UPDATE] {before} -> {after}"
                                            );
                                        }
                                    }
                                }
                            } else {
                                #[allow(clippy::print_stdout)]
                                {
                                    println!("[{reaction_name}]   [UPDATE] {before} -> {after}");
                                }
                            }
                        }
                        ResultDiff::Aggregation { .. } | ResultDiff::Noop => {
                            let result_json = serde_json::to_string(result)
                                .expect("ResultDiff serialization should succeed");
                            let operation = match result {
                                ResultDiff::Aggregation { .. } => "AGGREGATION",
                                ResultDiff::Noop => "NOOP",
                                _ => "UNKNOWN",
                            };
                            #[allow(clippy::print_stdout)]
                            {
                                println!("[{reaction_name}]   [{operation}] {result_json}");
                            }
                        }
                    }
                }

                // Capture reaction_complete_ns timestamp
                if let Some(ref mut profiling) = query_result.profiling {
                    profiling.reaction_complete_ns = Some(drasi_lib::profiling::timestamp_ns());

                    // Log profiling summary if available
                    if let (Some(source_send), Some(reaction_complete)) =
                        (profiling.source_send_ns, profiling.reaction_complete_ns)
                    {
                        let total_latency_ns = reaction_complete - source_send;
                        debug!(
                            "[{}] End-to-end latency: {:.2}ms",
                            reaction_name,
                            total_latency_ns as f64 / 1_000_000.0
                        );
                    }
                }
            }
        });

        // Store the processing task handle
        self.base.set_processing_task(processing_task).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Use ReactionBase common stop functionality
        self.base.stop_common().await?;

        // Transition to Stopped
        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("Log reaction stopped".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
}
