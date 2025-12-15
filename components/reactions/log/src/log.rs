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

use drasi_lib::channels::{ComponentEventSender, ComponentStatus};
use drasi_lib::managers::log_component_start;
use drasi_lib::plugin_core::{QuerySubscriber, Reaction};
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};

pub struct LogReaction {
    base: ReactionBase,
    config: LogReactionConfig,
}

impl LogReaction {
    /// Create a new log reaction
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn new(id: impl Into<String>, queries: Vec<String>, config: LogReactionConfig) -> Self {
        let id = id.into();
        let params = ReactionBaseParams::new(id, queries);
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    /// Create a new log reaction with custom priority queue capacity
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: LogReactionConfig,
        priority_queue_capacity: usize,
    ) -> Self {
        let id = id.into();
        let params = ReactionBaseParams::new(id, queries)
            .with_priority_queue_capacity(priority_queue_capacity);
        Self {
            base: ReactionBase::new(params),
            config,
        }
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

    /// Set Handlebars template for ADD events
    ///
    /// Available variables: `after`, `query_name`, `operation`
    ///
    /// Example: `"[NEW] Item {{after.id}}: {{after.name}}"`
    pub fn with_added_template(mut self, template: impl Into<String>) -> Self {
        self.config.added_template = Some(template.into());
        self
    }

    /// Set Handlebars template for UPDATE events
    ///
    /// Available variables: `before`, `after`, `data`, `query_name`, `operation`
    ///
    /// Example: `"[CHG] {{after.id}}: {{before.value}} -> {{after.value}}"`
    pub fn with_updated_template(mut self, template: impl Into<String>) -> Self {
        self.config.updated_template = Some(template.into());
        self
    }

    /// Set Handlebars template for DELETE events
    ///
    /// Available variables: `before`, `query_name`, `operation`
    ///
    /// Example: `"[DEL] Item {{before.id}} removed"`
    pub fn with_deleted_template(mut self, template: impl Into<String>) -> Self {
        self.config.deleted_template = Some(template.into());
        self
    }

    /// Set templates for a specific query
    ///
    /// This allows different formatting for different queries. Query-specific templates
    /// override the default templates set via `with_added_template`, `with_updated_template`,
    /// and `with_deleted_template`.
    ///
    /// # Arguments
    ///
    /// * `query_id` - The ID of the query to configure
    /// * `added` - Optional template for ADD operations from this query
    /// * `updated` - Optional template for UPDATE operations from this query
    /// * `deleted` - Optional template for DELETE operations from this query
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let reaction = LogReaction::builder("my-logger")
    ///     .from_query("sensor-query")
    ///     .from_query("user-query")
    ///     .with_added_template("[DEFAULT] {{after.id}}")  // Default for all queries
    ///     .with_query_templates(
    ///         "sensor-query",
    ///         Some("[SENSOR] {{after.id}}: {{after.temperature}}°C"),
    ///         Some("[SENSOR-UPD] {{after.id}}: {{before.temperature}}°C -> {{after.temperature}}°C"),
    ///         Some("[SENSOR-DEL] {{before.id}}")
    ///     )
    ///     .build();
    /// ```
    pub fn with_query_templates(
        mut self,
        query_id: impl Into<String>,
        added: Option<impl Into<String>>,
        updated: Option<impl Into<String>>,
        deleted: Option<impl Into<String>>,
    ) -> Self {
        use crate::config::{QueryTemplates, TemplateSpec};
        
        let templates = QueryTemplates {
            added: added.map(|t| TemplateSpec {
                template: t.into(),
            }),
            updated: updated.map(|t| TemplateSpec {
                template: t.into(),
            }),
            deleted: deleted.map(|t| TemplateSpec {
                template: t.into(),
            }),
        };
        
        self.config.query_templates.insert(query_id.into(), templates);
        self
    }

    /// Set ADD template for a specific query
    ///
    /// Convenience method to set only the ADD template for a query.
    pub fn with_query_added_template(
        mut self,
        query_id: impl Into<String>,
        template: impl Into<String>,
    ) -> Self {
        use crate::config::{TemplateSpec};
        
        let query_id = query_id.into();
        let entry = self.config.query_templates.entry(query_id).or_default();
        entry.added = Some(TemplateSpec {
            template: template.into(),
        });
        self
    }

    /// Set UPDATE template for a specific query
    ///
    /// Convenience method to set only the UPDATE template for a query.
    pub fn with_query_updated_template(
        mut self,
        query_id: impl Into<String>,
        template: impl Into<String>,
    ) -> Self {
        use crate::config::{TemplateSpec};
        
        let query_id = query_id.into();
        let entry = self.config.query_templates.entry(query_id).or_default();
        entry.updated = Some(TemplateSpec {
            template: template.into(),
        });
        self
    }

    /// Set DELETE template for a specific query
    ///
    /// Convenience method to set only the DELETE template for a query.
    pub fn with_query_deleted_template(
        mut self,
        query_id: impl Into<String>,
        template: impl Into<String>,
    ) -> Self {
        use crate::config::{TemplateSpec};
        
        let query_id = query_id.into();
        let entry = self.config.query_templates.entry(query_id).or_default();
        entry.deleted = Some(TemplateSpec {
            template: template.into(),
        });
        self
    }

    /// Build the LogReaction
    pub fn build(self) -> LogReaction {
        let mut params =
            ReactionBaseParams::new(self.id, self.queries).with_auto_start(self.auto_start);
        if let Some(capacity) = self.priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        LogReaction {
            base: ReactionBase::new(params),
            config: self.config,
        }
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

    async fn inject_query_subscriber(&self, query_subscriber: Arc<dyn QuerySubscriber>) {
        self.base.inject_query_subscriber(query_subscriber).await;
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
        // QuerySubscriber was injected via inject_query_subscriber() when reaction was added
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
                    if let Some(result_type) = result.get("type").and_then(|v| v.as_str()) {
                        // Normalize result_type to lowercase for matching
                        let result_type_lower = result_type.to_lowercase();

                        // Build context for template rendering
                        let mut context = Map::new();
                        context.insert(
                            "query_name".to_string(),
                            Value::String(query_result.query_id.clone()),
                        );
                        context.insert(
                            "operation".to_string(),
                            Value::String(result_type.to_uppercase()),
                        );

                        // Get query-specific templates if available
                        let query_templates = config.query_templates.get(&query_result.query_id);

                        match result_type_lower.as_str() {
                            "add" => {
                                if let Some(data) = result.get("data") {
                                    context.insert("after".to_string(), data.clone());

                                    // Check for query-specific template first, then default
                                    let template = query_templates
                                        .and_then(|qt| qt.added.as_ref())
                                        .map(|ts| ts.template.as_str())
                                        .or(config.added_template.as_deref());

                                    if let Some(template_str) = template {
                                        // Use template
                                        match handlebars.render_template(template_str, &context) {
                                            Ok(rendered) => {
                                                #[allow(clippy::print_stdout)]
                                                {
                                                    println!("[{reaction_name}]   {rendered}");
                                                }
                                            }
                                            Err(e) => {
                                                debug!(
                                                    "[{reaction_name}] Template render error: {e}"
                                                );
                                                // Fall back to JSON output
                                                #[allow(clippy::print_stdout)]
                                                {
                                                    println!("[{reaction_name}]   [ADD] {data}");
                                                }
                                            }
                                        }
                                    } else {
                                        // Default: show full JSON
                                        #[allow(clippy::print_stdout)]
                                        {
                                            println!("[{reaction_name}]   [ADD] {data}");
                                        }
                                    }
                                }
                            }
                            "remove" | "delete" => {
                                if let Some(data) = result.get("data") {
                                    context.insert("before".to_string(), data.clone());

                                    // Check for query-specific template first, then default
                                    let template = query_templates
                                        .and_then(|qt| qt.deleted.as_ref())
                                        .map(|ts| ts.template.as_str())
                                        .or(config.deleted_template.as_deref());

                                    if let Some(template_str) = template {
                                        // Use template
                                        match handlebars.render_template(template_str, &context) {
                                            Ok(rendered) => {
                                                #[allow(clippy::print_stdout)]
                                                {
                                                    println!("[{reaction_name}]   {rendered}");
                                                }
                                            }
                                            Err(e) => {
                                                debug!(
                                                    "[{reaction_name}] Template render error: {e}"
                                                );
                                                // Fall back to JSON output
                                                #[allow(clippy::print_stdout)]
                                                {
                                                    println!("[{reaction_name}]   [DELETE] {data}");
                                                }
                                            }
                                        }
                                    } else {
                                        // Default: show full JSON
                                        #[allow(clippy::print_stdout)]
                                        {
                                            println!("[{reaction_name}]   [DELETE] {data}");
                                        }
                                    }
                                }
                            }
                            "update" => {
                                if let (Some(before), Some(after)) =
                                    (result.get("before"), result.get("after"))
                                {
                                    context.insert("before".to_string(), before.clone());
                                    context.insert("after".to_string(), after.clone());
                                    if let Some(data) = result.get("data") {
                                        context.insert("data".to_string(), data.clone());
                                    }

                                    // Check for query-specific template first, then default
                                    let template = query_templates
                                        .and_then(|qt| qt.updated.as_ref())
                                        .map(|ts| ts.template.as_str())
                                        .or(config.updated_template.as_deref());

                                    if let Some(template_str) = template {
                                        // Use template
                                        match handlebars.render_template(template_str, &context) {
                                            Ok(rendered) => {
                                                #[allow(clippy::print_stdout)]
                                                {
                                                    println!("[{reaction_name}]   {rendered}");
                                                }
                                            }
                                            Err(e) => {
                                                debug!(
                                                    "[{reaction_name}] Template render error: {e}"
                                                );
                                                // Fall back to JSON output
                                                #[allow(clippy::print_stdout)]
                                                {
                                                    println!(
                                                        "[{reaction_name}]   [UPDATE] {before} -> {after}"
                                                    );
                                                }
                                            }
                                        }
                                    } else {
                                        // Default: show full JSON
                                        #[allow(clippy::print_stdout)]
                                        {
                                            println!("[{reaction_name}]   [UPDATE] {before} -> {after}");
                                        }
                                    }
                                }
                            }
                            _ => {
                                #[allow(clippy::print_stdout)]
                                {
                                    println!(
                                        "[{}]   [{}] {}",
                                        reaction_name,
                                        result_type.to_uppercase(),
                                        result
                                    );
                                }
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

    async fn inject_event_tx(&self, tx: ComponentEventSender) {
        self.base.inject_event_tx(tx).await;
    }
}
