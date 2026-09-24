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

use anyhow::Result;
use async_trait::async_trait;
use log::debug;
use serde_json::Value;
use std::collections::HashMap;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::Reaction;

use crate::config::LogReactionConfig;
use crate::render;

/// Reaction that writes continuous query results to the console (stdout).
///
/// Each emission is printed in timestamp order. Output can be customised with
/// per-query or default Handlebars templates; without a template the reaction
/// prints a human-readable line for every change.
pub struct LogReaction {
    pub(crate) base: ReactionBase,
    config: LogReactionConfig,
}

impl LogReaction {
    /// Create a new log reaction.
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to `DrasiLib` via `add_reaction()`.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for the reaction.
    /// * `queries` - Query ids to subscribe to.
    /// * `config` - Configuration including templates and routes.
    ///
    /// # Errors
    ///
    /// Returns an error if any template has invalid Handlebars syntax or if a
    /// route does not match any subscribed query.
    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: LogReactionConfig,
    ) -> anyhow::Result<Self> {
        config.validate(&queries)?;
        let params = ReactionBaseParams::new(id.into(), queries);
        Ok(Self {
            base: ReactionBase::new(params),
            config,
        })
    }

    /// Create a new log reaction with a custom priority queue capacity.
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to `DrasiLib` via `add_reaction()`.
    ///
    /// # Errors
    ///
    /// Returns an error if any template has invalid Handlebars syntax or if a
    /// route does not match any subscribed query.
    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: LogReactionConfig,
        priority_queue_capacity: usize,
    ) -> anyhow::Result<Self> {
        config.validate(&queries)?;
        let params = ReactionBaseParams::new(id.into(), queries)
            .with_priority_queue_capacity(priority_queue_capacity);
        Ok(Self {
            base: ReactionBase::new(params),
            config,
        })
    }

    /// Create a builder for [`LogReaction`].
    pub fn builder(id: impl Into<String>) -> LogReactionBuilder {
        LogReactionBuilder::new(id)
    }

    /// Mutable access to the underlying [`ReactionBase`].
    ///
    /// The dynamic-plugin descriptor uses this to record the raw configuration
    /// via `set_raw_config` after construction.
    pub fn base_mut(&mut self) -> &mut ReactionBase {
        &mut self.base
    }

    #[allow(clippy::print_stdout)]
    fn log_result(&self, message: &str) {
        println!("[{}] {}", self.base.id, message);
    }
}

/// Builder for [`LogReaction`].
pub struct LogReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: LogReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl LogReactionBuilder {
    /// Create a new builder with the given reaction id.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: LogReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    /// Set the query ids to subscribe to.
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    /// Add a query id to subscribe to.
    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Alias of [`with_query`](Self::with_query); reads naturally at call sites.
    pub fn from_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Set a custom priority queue capacity.
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set whether the reaction should auto-start.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Replace the entire configuration.
    pub fn with_config(mut self, config: LogReactionConfig) -> Self {
        self.config = config;
        self
    }

    /// Set the default template configuration for all queries.
    ///
    /// The default template is used when no query-specific route applies.
    ///
    /// # Example
    ///
    /// ```rust
    /// use drasi_reaction_log::{LogReaction, QueryConfig, TemplateSpec};
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
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn with_default_template(mut self, template: crate::config::QueryConfig) -> Self {
        self.config.default_template = Some(template);
        self
    }

    /// Set the template configuration for a specific query.
    ///
    /// Query-specific templates override the default template.
    ///
    /// # Example
    ///
    /// ```rust
    /// use drasi_reaction_log::{LogReaction, QueryConfig, TemplateSpec};
    ///
    /// let sensor_config = QueryConfig {
    ///     added: Some(TemplateSpec::new("[SENSOR] {{after.id}}: {{after.temperature}}")),
    ///     updated: None,
    ///     deleted: None,
    /// };
    ///
    /// let reaction = LogReaction::builder("my-logger")
    ///     .with_query("sensor-data")
    ///     .with_route("sensor-data", sensor_config)
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn with_route(
        mut self,
        query_id: impl Into<String>,
        config: crate::config::QueryConfig,
    ) -> Self {
        self.config.routes.insert(query_id.into(), config);
        self
    }

    /// Build the [`LogReaction`].
    ///
    /// # Errors
    ///
    /// Returns an error if any template has invalid Handlebars syntax or if a
    /// route does not match any subscribed query.
    pub fn build(self) -> anyhow::Result<LogReaction> {
        self.config.validate(&self.queries)?;

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
        use crate::descriptor::{LogReactionConfigDto, QueryConfigDto, TemplateSpecDto};

        fn map_spec_to_dto(spec: &crate::TemplateSpec) -> TemplateSpecDto {
            TemplateSpecDto {
                template: spec.template.clone(),
            }
        }

        fn map_qc_to_dto(qc: &crate::QueryConfig) -> QueryConfigDto {
            QueryConfigDto {
                added: qc.added.as_ref().map(map_spec_to_dto),
                updated: qc.updated.as_ref().map(map_spec_to_dto),
                deleted: qc.deleted.as_ref().map(map_spec_to_dto),
            }
        }

        let dto = LogReactionConfigDto {
            routes: self
                .config
                .routes
                .iter()
                .map(|(k, v)| (k.clone(), map_qc_to_dto(v)))
                .collect(),
            default_template: self.config.default_template.as_ref().map(map_qc_to_dto),
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
        log_component_start("Reaction", &self.base.id);

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting log reaction".to_string()),
            )
            .await;

        self.base
            .set_status(
                ComponentStatus::Running,
                Some("Log reaction started".to_string()),
            )
            .await;

        self.log_result(&format!(
            "Started - receiving results from queries: {:?}",
            self.base.queries
        ));

        // Spawn the processing task to dequeue and process results in
        // timestamp order. `start()` itself must return promptly.
        let priority_queue = self.base.priority_queue.clone();
        let reaction_name = self.base.id.clone();
        let config = self.config.clone();

        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        let processing_task = tokio::spawn(async move {
            let handlebars = render::build_handlebars();

            loop {
                let query_result_arc = tokio::select! {
                    biased;

                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_name}] Received shutdown signal, exiting processing loop");
                        break;
                    }

                    result = priority_queue.dequeue() => result,
                };

                // Clone so we can record profiling timestamps; `Arc` is shared.
                let mut query_result = (*query_result_arc).clone();

                if let Some(ref mut profiling) = query_result.profiling {
                    profiling.reaction_receive_ns = Some(drasi_lib::profiling::timestamp_ns());
                }

                // Treat an empty result set as a no-op.
                if query_result.results.is_empty() {
                    debug!("[{reaction_name}] Received empty result set from query");
                    continue;
                }

                let timestamp = query_result.timestamp.to_rfc3339();
                let metadata = Value::Object(query_result.metadata.clone().into_iter().collect());

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
                    if let Some(line) = render::render_diff(
                        &config,
                        &handlebars,
                        &query_result.query_id,
                        &timestamp,
                        query_result.sequence,
                        &metadata,
                        result,
                    ) {
                        #[allow(clippy::print_stdout)]
                        {
                            println!("[{reaction_name}]   {line}");
                        }
                    }
                }

                if let Some(ref mut profiling) = query_result.profiling {
                    profiling.reaction_complete_ns = Some(drasi_lib::profiling::timestamp_ns());

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

        self.base.set_processing_task(processing_task).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await?;

        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("Log reaction stopped".to_string()),
            )
            .await;

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
        drasi_lib::recovery::ReactionRecoveryPolicy::AutoSkipGap
    }
}
