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

use std::sync::Arc;

use futures::stream::{self, Stream};

use crate::channels::ComponentEvent;
use crate::config::{DrasiLibConfig, RuntimeConfig};
use crate::error::DrasiError;
use crate::queries::QueryManager;
use crate::reactions::ReactionManager;
use crate::sources::SourceManager;
use crate::state_guard::StateGuard;

/// Classify an `anyhow::Error` from a manager call: if it wraps a
/// `ComponentNotFoundError`, return `ComponentNotFound`; otherwise preserve
/// the real failure as `OperationFailed`.
fn classify_component_error(
    e: anyhow::Error,
    component_type: &str,
    component_id: &str,
    operation: &str,
) -> DrasiError {
    if let Some(not_found) = e.downcast_ref::<crate::managers::ComponentNotFoundError>() {
        DrasiError::component_not_found(not_found.component_type, &not_found.component_id)
    } else {
        DrasiError::operation_failed(component_type, component_id, operation, e.to_string())
    }
}

/// Inspection API for querying server state and component information
///
/// This module provides all inspection/listing methods for sources, queries, and reactions,
/// separated from the main server core lifecycle management.
#[derive(Clone)]
pub struct InspectionAPI {
    source_manager: Arc<SourceManager>,
    query_manager: Arc<QueryManager>,
    reaction_manager: Arc<ReactionManager>,
    state_guard: StateGuard,
    config: Arc<RuntimeConfig>,
}

impl InspectionAPI {
    pub(crate) fn new(
        source_manager: Arc<SourceManager>,
        query_manager: Arc<QueryManager>,
        reaction_manager: Arc<ReactionManager>,
        state_guard: StateGuard,
        config: Arc<RuntimeConfig>,
    ) -> Self {
        Self {
            source_manager,
            query_manager,
            reaction_manager,
            state_guard,
            config,
        }
    }

    // ============================================================================
    // Source Inspection Methods
    // ============================================================================

    /// List all sources with their current status
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let sources = core.list_sources().await?;
    /// for (id, status) in sources {
    ///     println!("Source {}: {:?}", id, status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_sources(
        &self,
    ) -> crate::error::Result<Vec<(String, crate::channels::ComponentStatus)>> {
        self.state_guard.require_initialized()?;
        Ok(self.source_manager.list_sources().await)
    }

    /// Get detailed information about a specific source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let source_info = core.get_source_info("my-source").await?;
    /// println!("Source type: {}", source_info.source_type);
    /// println!("Status: {:?}", source_info.status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_source_info(
        &self,
        id: &str,
    ) -> crate::error::Result<crate::config::SourceRuntime> {
        self.state_guard.require_initialized()?;
        self.source_manager
            .get_source(id.to_string())
            .await
            .map_err(|e| classify_component_error(e, "source", id, "get_info"))
    }

    /// Get the current status of a specific source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let status = core.get_source_status("my-source").await?;
    /// println!("Source status: {:?}", status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_source_status(
        &self,
        id: &str,
    ) -> crate::error::Result<crate::channels::ComponentStatus> {
        self.state_guard.require_initialized()?;
        self.source_manager
            .get_source_status(id.to_string())
            .await
            .map_err(|e| classify_component_error(e, "source", id, "get_status"))
    }

    /// Get best-effort schema information for a specific source.
    pub async fn get_source_schema(
        &self,
        id: &str,
    ) -> crate::error::Result<Option<crate::config::SourceSchema>> {
        self.state_guard.require_initialized()?;
        self.source_manager
            .get_source_schema(id.to_string())
            .await
            .map_err(|e| classify_component_error(e, "source", id, "get_schema"))
    }

    /// Get the merged graph schema across all registered sources and queries.
    pub async fn get_graph_schema(&self) -> crate::error::Result<crate::config::GraphSchema> {
        self.state_guard.require_initialized()?;

        let mut schema = crate::config::GraphSchema::default();

        for (source_id, _) in self.source_manager.list_sources().await {
            if source_id == crate::sources::COMPONENT_GRAPH_SOURCE_ID {
                continue;
            }

            match self
                .source_manager
                .get_source_schema(source_id.clone())
                .await
            {
                Ok(Some(source_schema)) if !source_schema.is_empty() => {
                    schema.merge_source_schema(&source_id, &source_schema);
                }
                Ok(_) => schema.record_source_without_schema(&source_id),
                Err(e) => {
                    log::warn!("Failed to inspect schema for source '{source_id}': {e}");
                    schema.record_source_without_schema(&source_id);
                }
            }
        }

        for (query_id, _) in self.query_manager.list_queries().await {
            if let Some(config) = self.query_manager.get_query_config(&query_id).await {
                match crate::queries::label_extractor::LabelExtractor::extract_labels(
                    &config.query,
                    &config.query_language,
                ) {
                    Ok(labels) => {
                        schema.mark_queried_nodes(
                            labels.node_labels.iter().map(|label| label.as_str()),
                            &query_id,
                        );
                        schema.mark_queried_relations(
                            labels.relation_labels.iter().map(|label| label.as_str()),
                            &query_id,
                        );
                    }
                    Err(e) => {
                        log::warn!("Failed to extract labels for query '{query_id}': {e}");
                    }
                }
            }
        }

        Ok(schema)
    }

    // ============================================================================
    // Query Inspection Methods
    // ============================================================================

    /// List all queries with their current status
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let queries = core.list_queries().await?;
    /// for (id, status) in queries {
    ///     println!("Query {}: {:?}", id, status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_queries(
        &self,
    ) -> crate::error::Result<Vec<(String, crate::channels::ComponentStatus)>> {
        self.state_guard.require_initialized()?;
        Ok(self.query_manager.list_queries().await)
    }

    /// Get detailed information about a specific query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let query_info = core.get_query_info("my-query").await?;
    /// println!("Query: {}", query_info.query);
    /// println!("Status: {:?}", query_info.status);
    /// println!("Source subscriptions: {:?}", query_info.source_subscriptions);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_info(
        &self,
        id: &str,
    ) -> crate::error::Result<crate::config::QueryRuntime> {
        self.state_guard.require_initialized()?;
        self.query_manager
            .get_query(id.to_string())
            .await
            .map_err(|e| classify_component_error(e, "query", id, "get_info"))
    }

    /// Get the current status of a specific query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let status = core.get_query_status("my-query").await?;
    /// println!("Query status: {:?}", status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_status(
        &self,
        id: &str,
    ) -> crate::error::Result<crate::channels::ComponentStatus> {
        self.state_guard.require_initialized()?;
        self.query_manager
            .get_query_status(id.to_string())
            .await
            .map_err(|e| classify_component_error(e, "query", id, "get_status"))
    }

    /// Get the current result set for a running query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let results = core.get_query_results("my-query").await?;
    /// println!("Current results: {} items", results.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_results(
        &self,
        id: &str,
    ) -> crate::error::Result<Vec<serde_json::Value>> {
        self.state_guard.require_initialized()?;

        // Check preconditions explicitly instead of parsing error strings.
        // First verify the query exists via its status.
        let status = self
            .query_manager
            .get_query_status(id.to_string())
            .await
            .map_err(|e| classify_component_error(e, "query", id, "get_status"))?;

        if status != crate::channels::ComponentStatus::Running {
            return Err(DrasiError::invalid_state(format!(
                "Query '{id}' is not running"
            )));
        }

        self.query_manager
            .get_query_results(id)
            .await
            .map_err(|e| DrasiError::operation_failed("query", id, "get_results", e.to_string()))
    }

    /// Get the full configuration for a specific query
    ///
    /// This returns the complete query configuration including all fields like auto_start and joins,
    /// unlike `get_query_info()` which only returns runtime information.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let config = core.get_query_config("my-query").await?;
    /// println!("Auto-start: {}", config.auto_start);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_config(
        &self,
        id: &str,
    ) -> crate::error::Result<crate::config::QueryConfig> {
        self.state_guard.require_initialized()?;
        self.query_manager
            .get_query_config(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("query", id))
    }

    // ============================================================================
    // Reaction Inspection Methods
    // ============================================================================

    /// List all reactions with their current status
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let reactions = core.list_reactions().await?;
    /// for (id, status) in reactions {
    ///     println!("Reaction {}: {:?}", id, status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_reactions(
        &self,
    ) -> crate::error::Result<Vec<(String, crate::channels::ComponentStatus)>> {
        self.state_guard.require_initialized()?;
        Ok(self.reaction_manager.list_reactions().await)
    }

    /// Get detailed information about a specific reaction
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let reaction_info = core.get_reaction_info("my-reaction").await?;
    /// println!("Reaction type: {}", reaction_info.reaction_type);
    /// println!("Status: {:?}", reaction_info.status);
    /// println!("Queries: {:?}", reaction_info.queries);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_reaction_info(
        &self,
        id: &str,
    ) -> crate::error::Result<crate::config::ReactionRuntime> {
        self.state_guard.require_initialized()?;
        self.reaction_manager
            .get_reaction(id.to_string())
            .await
            .map_err(|e| classify_component_error(e, "reaction", id, "get_info"))
    }

    /// Get the current status of a specific reaction
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let status = core.get_reaction_status("my-reaction").await?;
    /// println!("Reaction status: {:?}", status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_reaction_status(
        &self,
        id: &str,
    ) -> crate::error::Result<crate::channels::ComponentStatus> {
        self.state_guard.require_initialized()?;
        self.reaction_manager
            .get_reaction_status(id.to_string())
            .await
            .map_err(|e| classify_component_error(e, "reaction", id, "get_status"))
    }

    // ============================================================================
    // Full Configuration Snapshot
    // ============================================================================

    /// Get a complete configuration snapshot of all components
    ///
    /// Returns the full server configuration including all queries with their complete configurations.
    /// Note: Sources and reactions are now instance-only and don't have stored configs.
    /// Use `list_sources()` and `list_reactions()` to get runtime information about these components.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let config = core.get_current_config().await?;
    /// println!("Server has {} queries", config.queries.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_current_config(&self) -> crate::error::Result<DrasiLibConfig> {
        self.state_guard.require_initialized()?;

        // Collect all query configs
        let query_ids: Vec<String> = self
            .query_manager
            .list_queries()
            .await
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        let mut queries = Vec::new();
        for id in query_ids {
            if let Some(config) = self.query_manager.get_query_config(&id).await {
                queries.push(config);
            }
        }

        Ok(DrasiLibConfig {
            id: self.config.id.clone(),
            priority_queue_capacity: self.config.global_priority_queue_capacity,
            dispatch_buffer_capacity: self.config.global_dispatch_buffer_capacity,
            storage_backends: self.config.storage_backends.clone(),
            queries,
        })
    }

    // ============================================================================
    // Component Event History Methods
    // ============================================================================

    /// Get events for a specific source as an async stream.
    ///
    /// Returns events in chronological order (oldest first).
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # use futures::StreamExt;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut events = core.get_source_events("my-source").await?;
    /// while let Some(event) = events.next().await {
    ///     println!("Event: {:?} - {:?}", event.status, event.message);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_source_events(
        &self,
        id: &str,
    ) -> crate::error::Result<impl Stream<Item = ComponentEvent>> {
        self.state_guard.require_initialized()?;
        let events = self.source_manager.get_source_events(id).await;
        Ok(stream::iter(events))
    }

    /// Get events for a specific query as an async stream.
    ///
    /// Returns events in chronological order (oldest first).
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # use futures::StreamExt;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut events = core.get_query_events("my-query").await?;
    /// while let Some(event) = events.next().await {
    ///     println!("Event: {:?} - {:?}", event.status, event.message);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_events(
        &self,
        id: &str,
    ) -> crate::error::Result<impl Stream<Item = ComponentEvent>> {
        self.state_guard.require_initialized()?;
        let events = self.query_manager.get_query_events(id).await;
        Ok(stream::iter(events))
    }

    /// Get events for a specific reaction as an async stream.
    ///
    /// Returns events in chronological order (oldest first).
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # use futures::StreamExt;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut events = core.get_reaction_events("my-reaction").await?;
    /// while let Some(event) = events.next().await {
    ///     println!("Event: {:?} - {:?}", event.status, event.message);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_reaction_events(
        &self,
        id: &str,
    ) -> crate::error::Result<impl Stream<Item = ComponentEvent>> {
        self.state_guard.require_initialized()?;
        let events = self.reaction_manager.get_reaction_events(id).await;
        Ok(stream::iter(events))
    }

    /// Get all events across all sources as an async stream.
    ///
    /// Returns events sorted by timestamp (oldest first).
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # use futures::StreamExt;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut events = core.get_all_source_events().await?;
    /// while let Some(event) = events.next().await {
    ///     println!("{}: {:?}", event.component_id, event.status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_all_source_events(
        &self,
    ) -> crate::error::Result<impl Stream<Item = ComponentEvent>> {
        self.state_guard.require_initialized()?;
        let events = self.source_manager.get_all_events().await;
        Ok(stream::iter(events))
    }

    /// Get all events across all queries as an async stream.
    ///
    /// Returns events sorted by timestamp (oldest first).
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # use futures::StreamExt;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut events = core.get_all_query_events().await?;
    /// while let Some(event) = events.next().await {
    ///     println!("{}: {:?}", event.component_id, event.status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_all_query_events(
        &self,
    ) -> crate::error::Result<impl Stream<Item = ComponentEvent>> {
        self.state_guard.require_initialized()?;
        let events = self.query_manager.get_all_events().await;
        Ok(stream::iter(events))
    }

    /// Get all events across all reactions as an async stream.
    ///
    /// Returns events sorted by timestamp (oldest first).
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # use futures::StreamExt;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut events = core.get_all_reaction_events().await?;
    /// while let Some(event) = events.next().await {
    ///     println!("{}: {:?}", event.component_id, event.status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_all_reaction_events(
        &self,
    ) -> crate::error::Result<impl Stream<Item = ComponentEvent>> {
        self.state_guard.require_initialized()?;
        let events = self.reaction_manager.get_all_events().await;
        Ok(stream::iter(events))
    }

    /// Get all events across all components (sources, queries, reactions) as an async stream.
    ///
    /// Returns events sorted by timestamp (oldest first).
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # use futures::StreamExt;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut events = core.get_all_events().await?;
    /// while let Some(event) = events.next().await {
    ///     println!("{} ({:?}): {:?}", event.component_id, event.component_type, event.status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_all_events(&self) -> crate::error::Result<impl Stream<Item = ComponentEvent>> {
        self.state_guard.require_initialized()?;

        // Collect events from all managers
        let mut all_events = Vec::new();
        all_events.extend(self.source_manager.get_all_events().await);
        all_events.extend(self.query_manager.get_all_events().await);
        all_events.extend(self.reaction_manager.get_all_events().await);

        // Sort by timestamp
        all_events.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

        Ok(stream::iter(all_events))
    }

    // ============================================================================
    // Log Subscription Methods
    // ============================================================================

    /// Subscribe to live logs for a source.
    ///
    /// Returns the log history and a broadcast receiver for new logs.
    /// The receiver will receive new log messages as they are emitted by the source.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let (history, mut receiver) = core.subscribe_source_logs("my-source").await?;
    ///
    /// // Print historical logs
    /// for log in history {
    ///     println!("[{:?}] {}", log.level, log.message);
    /// }
    ///
    /// // Listen for new logs
    /// while let Ok(log) = receiver.recv().await {
    ///     println!("[{:?}] {}", log.level, log.message);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn subscribe_source_logs(
        &self,
        id: &str,
    ) -> crate::error::Result<(
        Vec<crate::managers::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::managers::LogMessage>,
    )> {
        self.state_guard.require_initialized()?;
        self.source_manager
            .subscribe_logs(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("source", id))
    }

    /// Subscribe to live logs for a query.
    ///
    /// Returns the log history and a broadcast receiver for new logs.
    /// The receiver will receive new log messages as they are emitted by the query.
    pub async fn subscribe_query_logs(
        &self,
        id: &str,
    ) -> crate::error::Result<(
        Vec<crate::managers::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::managers::LogMessage>,
    )> {
        self.state_guard.require_initialized()?;
        self.query_manager
            .subscribe_logs(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("query", id))
    }

    /// Subscribe to live logs for a reaction.
    ///
    /// Returns the log history and a broadcast receiver for new logs.
    /// The receiver will receive new log messages as they are emitted by the reaction.
    pub async fn subscribe_reaction_logs(
        &self,
        id: &str,
    ) -> crate::error::Result<(
        Vec<crate::managers::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::managers::LogMessage>,
    )> {
        self.state_guard.require_initialized()?;
        self.reaction_manager
            .subscribe_logs(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("reaction", id))
    }

    /// Subscribe to live events for a source.
    ///
    /// Returns the event history and a broadcast receiver for new events.
    /// The receiver will receive new lifecycle events as they occur (e.g., status changes).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// # async fn example(core: &DrasiLib) -> anyhow::Result<()> {
    /// let (history, mut receiver) = core.subscribe_source_events("my-source").await?;
    ///
    /// // Print historical events
    /// for event in history {
    ///     println!("[{:?}] {:?}: {:?}", event.timestamp, event.status, event.message);
    /// }
    ///
    /// // Listen for new events
    /// while let Ok(event) = receiver.recv().await {
    ///     println!("[{:?}] {:?}: {:?}", event.timestamp, event.status, event.message);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn subscribe_source_events(
        &self,
        id: &str,
    ) -> crate::error::Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        self.state_guard.require_initialized()?;
        self.source_manager
            .subscribe_events(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("source", id))
    }

    /// Subscribe to live events for a query.
    ///
    /// Returns the event history and a broadcast receiver for new events.
    /// The receiver will receive new lifecycle events as they occur (e.g., status changes).
    pub async fn subscribe_query_events(
        &self,
        id: &str,
    ) -> crate::error::Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        self.state_guard.require_initialized()?;
        self.query_manager
            .subscribe_events(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("query", id))
    }

    /// Subscribe to live events for a reaction.
    ///
    /// Returns the event history and a broadcast receiver for new events.
    /// The receiver will receive new lifecycle events as they occur (e.g., status changes).
    pub async fn subscribe_reaction_events(
        &self,
        id: &str,
    ) -> crate::error::Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        self.state_guard.require_initialized()?;
        self.reaction_manager
            .subscribe_events(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("reaction", id))
    }
}

#[cfg(test)]
mod tests {
    use crate::builder::Query;
    use crate::channels::*;
    use crate::error::DrasiError;
    use crate::lib_core::DrasiLib;
    use crate::sources::tests::TestMockSource;
    use crate::sources::COMPONENT_GRAPH_SOURCE_ID;

    use async_trait::async_trait;
    use std::collections::HashMap;

    // ========================================================================
    // Mock reaction for testing
    // ========================================================================

    struct TestMockReaction {
        id: String,
        queries: Vec<String>,
        auto_start: bool,
        status_handle: crate::component_graph::ComponentStatusHandle,
    }

    impl TestMockReaction {
        fn new(id: String, queries: Vec<String>, auto_start: bool) -> Self {
            let status_handle = crate::component_graph::ComponentStatusHandle::new(&id);
            Self {
                id,
                queries,
                auto_start,
                status_handle,
            }
        }
    }

    #[async_trait]
    impl crate::reactions::Reaction for TestMockReaction {
        fn id(&self) -> &str {
            &self.id
        }

        fn type_name(&self) -> &str {
            "mock"
        }

        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }

        fn query_ids(&self) -> Vec<String> {
            self.queries.clone()
        }

        fn auto_start(&self) -> bool {
            self.auto_start
        }

        async fn initialize(&self, context: crate::context::ReactionRuntimeContext) {
            self.status_handle.wire(context.update_tx.clone()).await;
        }

        async fn start(&self) -> anyhow::Result<()> {
            self.status_handle
                .set_status(
                    ComponentStatus::Starting,
                    Some("Starting reaction".to_string()),
                )
                .await;
            self.status_handle
                .set_status(
                    ComponentStatus::Running,
                    Some("Reaction started".to_string()),
                )
                .await;
            Ok(())
        }

        async fn stop(&self) -> anyhow::Result<()> {
            self.status_handle
                .set_status(
                    ComponentStatus::Stopping,
                    Some("Stopping reaction".to_string()),
                )
                .await;
            self.status_handle
                .set_status(
                    ComponentStatus::Stopped,
                    Some("Reaction stopped".to_string()),
                )
                .await;
            Ok(())
        }

        async fn status(&self) -> ComponentStatus {
            self.status_handle.get_status().await
        }

        async fn enqueue_query_result(&self, _result: QueryResult) -> anyhow::Result<()> {
            Ok(())
        }
    }

    // ========================================================================
    // Helpers
    // ========================================================================

    async fn build_and_start() -> DrasiLib {
        let core = DrasiLib::builder().with_id("test").build().await.unwrap();
        core.start().await.unwrap();
        core
    }

    async fn build_with_source_and_query() -> DrasiLib {
        let source = TestMockSource::new("test-source".to_string()).unwrap();
        let core = DrasiLib::builder()
            .with_id("test")
            .with_source(source)
            .with_query(
                Query::cypher("q1")
                    .query("MATCH (n:Test) RETURN n")
                    .from_source("test-source")
                    .auto_start(false)
                    .build(),
            )
            .build()
            .await
            .unwrap();
        core.start().await.unwrap();
        core
    }

    // ========================================================================
    // list_sources
    // ========================================================================

    #[tokio::test]
    async fn list_sources_empty() {
        let core = build_and_start().await;
        let sources = core.list_sources().await.unwrap();
        let user_sources: Vec<_> = sources
            .iter()
            .filter(|(id, _)| id != COMPONENT_GRAPH_SOURCE_ID)
            .collect();
        assert!(user_sources.is_empty(), "No user sources initially");
    }

    #[tokio::test]
    async fn list_sources_after_adding() {
        let core = build_and_start().await;
        let s1 = TestMockSource::with_auto_start("src-a".to_string(), false).unwrap();
        let s2 = TestMockSource::with_auto_start("src-b".to_string(), false).unwrap();
        core.add_source(s1).await.unwrap();
        core.add_source(s2).await.unwrap();

        let sources = core.list_sources().await.unwrap();
        let user_sources: Vec<_> = sources
            .iter()
            .filter(|(id, _)| id != COMPONENT_GRAPH_SOURCE_ID)
            .collect();
        assert_eq!(user_sources.len(), 2);
        assert!(user_sources.iter().any(|(id, _)| id == "src-a"));
        assert!(user_sources.iter().any(|(id, _)| id == "src-b"));
    }

    // ========================================================================
    // get_source_status
    // ========================================================================

    #[tokio::test]
    async fn get_source_status_found() {
        let core = build_and_start().await;
        let source = TestMockSource::with_auto_start("status-src".to_string(), false).unwrap();
        core.add_source(source).await.unwrap();

        let status = core.get_source_status("status-src").await.unwrap();
        assert_eq!(status, ComponentStatus::Added);
    }

    #[tokio::test]
    async fn get_source_status_not_found() {
        let core = build_and_start().await;
        let err = core.get_source_status("nonexistent").await.unwrap_err();
        assert!(
            matches!(err, DrasiError::ComponentNotFound { .. }),
            "Expected ComponentNotFound, got: {err:?}"
        );
    }

    // ========================================================================
    // list_queries
    // ========================================================================

    #[tokio::test]
    async fn list_queries_empty() {
        let core = build_and_start().await;
        let queries = core.list_queries().await.unwrap();
        assert!(queries.is_empty(), "No queries initially");
    }

    #[tokio::test]
    async fn list_queries_after_adding() {
        let core = build_with_source_and_query().await;
        let queries = core.list_queries().await.unwrap();
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].0, "q1");
    }

    // ========================================================================
    // get_query_status
    // ========================================================================

    #[tokio::test]
    async fn get_query_status_found() {
        let core = build_with_source_and_query().await;
        let status = core.get_query_status("q1").await.unwrap();
        assert_eq!(status, ComponentStatus::Added);
    }

    #[tokio::test]
    async fn get_query_status_not_found() {
        let core = build_and_start().await;
        let err = core.get_query_status("nonexistent").await.unwrap_err();
        assert!(
            matches!(err, DrasiError::ComponentNotFound { .. }),
            "Expected ComponentNotFound, got: {err:?}"
        );
    }

    // ========================================================================
    // list_reactions
    // ========================================================================

    #[tokio::test]
    async fn list_reactions_empty() {
        let core = build_and_start().await;
        let reactions = core.list_reactions().await.unwrap();
        assert!(reactions.is_empty(), "No reactions initially");
    }

    #[tokio::test]
    async fn list_reactions_after_adding() {
        let core = build_with_source_and_query().await;
        let reaction = TestMockReaction::new("r1".into(), vec!["q1".into()], false);
        core.add_reaction(reaction).await.unwrap();

        let reactions = core.list_reactions().await.unwrap();
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].0, "r1");
    }

    // ========================================================================
    // get_reaction_status
    // ========================================================================

    #[tokio::test]
    async fn get_reaction_status_found() {
        let core = build_with_source_and_query().await;
        let reaction = TestMockReaction::new("r1".into(), vec!["q1".into()], false);
        core.add_reaction(reaction).await.unwrap();

        let status = core.get_reaction_status("r1").await.unwrap();
        assert_eq!(status, ComponentStatus::Added);
    }

    #[tokio::test]
    async fn get_reaction_status_not_found() {
        let core = build_and_start().await;
        let err = core.get_reaction_status("nonexistent").await.unwrap_err();
        assert!(
            matches!(err, DrasiError::ComponentNotFound { .. }),
            "Expected ComponentNotFound, got: {err:?}"
        );
    }

    // ========================================================================
    // get_current_config
    // ========================================================================

    #[tokio::test]
    async fn get_current_config_returns_snapshot() {
        let core = build_with_source_and_query().await;
        let config = core.get_current_config().await.unwrap();
        assert_eq!(config.id, "test");
        assert_eq!(config.queries.len(), 1);
        assert_eq!(config.queries[0].id, "q1");
    }

    #[tokio::test]
    async fn get_current_config_empty_queries() {
        let core = build_and_start().await;
        let config = core.get_current_config().await.unwrap();
        assert_eq!(config.id, "test");
        assert!(config.queries.is_empty());
    }

    // ========================================================================
    // subscribe_source_logs — returns history + receiver
    // ========================================================================

    #[tokio::test]
    async fn subscribe_source_logs_returns_history_and_receiver() {
        let core = build_and_start().await;
        let source = TestMockSource::with_auto_start("log-src".to_string(), false).unwrap();
        core.add_source(source).await.unwrap();

        let (history, _receiver) = core.subscribe_source_logs("log-src").await.unwrap();
        // Freshly added source should have no log history yet
        assert!(history.is_empty(), "No logs emitted yet");
    }

    #[tokio::test]
    async fn subscribe_source_logs_not_found() {
        let core = build_and_start().await;
        let err = core.subscribe_source_logs("ghost").await.unwrap_err();
        assert!(
            matches!(err, DrasiError::ComponentNotFound { .. }),
            "Expected ComponentNotFound, got: {err:?}"
        );
    }

    // ========================================================================
    // subscribe_source_events — returns history + receiver
    // ========================================================================

    #[tokio::test]
    async fn subscribe_source_events_returns_history_and_receiver() {
        let core = build_and_start().await;
        let source = TestMockSource::with_auto_start("event-src".to_string(), false).unwrap();
        core.add_source(source).await.unwrap();

        let (history, _receiver) = core.subscribe_source_events("event-src").await.unwrap();
        // Newly added source should have at least a Stopped event from registration
        // (or could be empty depending on implementation). Just verify it returns successfully.
        let _ = history;
    }

    #[tokio::test]
    async fn subscribe_source_events_not_found() {
        let core = build_and_start().await;
        let err = core.subscribe_source_events("ghost").await.unwrap_err();
        assert!(
            matches!(err, DrasiError::ComponentNotFound { .. }),
            "Expected ComponentNotFound, got: {err:?}"
        );
    }
}
