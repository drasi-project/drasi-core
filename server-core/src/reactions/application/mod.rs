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

mod subscription;

pub use subscription::{Subscription, SubscriptionOptions, SubscriptionStream};

use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info};
use std::sync::Arc;
// RecvError no longer needed with trait-based receivers
use tokio::sync::{mpsc, RwLock};

use crate::channels::{ComponentEventSender, ComponentStatus, QueryResult};
use crate::config::ReactionConfig;
use crate::reactions::base::ReactionBase;
use crate::reactions::Reaction;
use crate::server_core::DrasiServerCore;
use crate::utils::log_component_start;

/// Handle for applications to receive query results from the ApplicationReaction
#[derive(Clone)]
pub struct ApplicationReactionHandle {
    rx: Arc<RwLock<Option<mpsc::Receiver<QueryResult>>>>,
    reaction_id: String,
}

impl ApplicationReactionHandle {
    /// Take the receiver (can only be called once)
    pub async fn take_receiver(&self) -> Option<mpsc::Receiver<QueryResult>> {
        self.rx.write().await.take()
    }

    /// Subscribe to query results with a callback
    pub async fn subscribe<F>(&self, mut callback: F) -> Result<()>
    where
        F: FnMut(QueryResult) + Send + 'static,
    {
        if let Some(mut rx) = self.take_receiver().await {
            tokio::spawn(async move {
                while let Some(result) = rx.recv().await {
                    callback(result);
                }
            });
            Ok(())
        } else {
            Err(anyhow::anyhow!("Receiver already taken"))
        }
    }

    /// Subscribe to query results with filtering
    pub async fn subscribe_filtered<F>(
        &self,
        query_filter: Vec<String>,
        mut callback: F,
    ) -> Result<()>
    where
        F: FnMut(QueryResult) + Send + 'static,
    {
        if let Some(mut rx) = self.take_receiver().await {
            tokio::spawn(async move {
                while let Some(result) = rx.recv().await {
                    if query_filter.is_empty() || query_filter.contains(&result.query_id) {
                        callback(result);
                    }
                }
            });
            Ok(())
        } else {
            Err(anyhow::anyhow!("Receiver already taken"))
        }
    }

    /// Subscribe to query results as a stream (for async iteration)
    pub async fn as_stream(&self) -> Option<ResultStream> {
        self.take_receiver().await.map(ResultStream::new)
    }

    /// Create a subscription with options
    pub async fn subscribe_with_options(
        &self,
        options: SubscriptionOptions,
    ) -> Result<Subscription> {
        if let Some(rx) = self.take_receiver().await {
            Ok(Subscription::new(rx, options))
        } else {
            Err(anyhow::anyhow!("Receiver already taken"))
        }
    }

    /// Get the reaction id for reference
    pub fn reaction_id(&self) -> &str {
        &self.reaction_id
    }
}

/// Stream wrapper for async iteration over query results
pub struct ResultStream {
    rx: mpsc::Receiver<QueryResult>,
}

impl ResultStream {
    fn new(rx: mpsc::Receiver<QueryResult>) -> Self {
        Self { rx }
    }

    /// Receive the next result
    pub async fn next(&mut self) -> Option<QueryResult> {
        self.rx.recv().await
    }

    /// Try to receive the next result without waiting
    pub fn try_next(&mut self) -> Option<QueryResult> {
        self.rx.try_recv().ok()
    }
}

/// A reaction that sends query results to the host application
pub struct ApplicationReaction {
    base: ReactionBase,
    app_tx: mpsc::Sender<QueryResult>,
}

impl ApplicationReaction {
    pub fn new(
        config: ReactionConfig,
        event_tx: ComponentEventSender,
    ) -> (Self, ApplicationReactionHandle) {
        let (app_tx, app_rx) = mpsc::channel(1000);

        let handle = ApplicationReactionHandle {
            rx: Arc::new(RwLock::new(Some(app_rx))),
            reaction_id: config.id.clone(),
        };

        let reaction = Self {
            base: ReactionBase::new(config, event_tx),
            app_tx,
        };

        (reaction, handle)
    }
}

#[async_trait]
impl Reaction for ApplicationReaction {
    async fn start(&self, server_core: Arc<DrasiServerCore>) -> Result<()> {
        log_component_start("Reaction", &self.base.config.id);

        // Transition to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting application reaction".to_string()),
            )
            .await?;

        // Subscribe to all configured queries using ReactionBase
        self.base.subscribe_to_queries(server_core).await?;

        // Transition to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("Application reaction started".to_string()),
            )
            .await?;

        // Spawn processing task to dequeue and process results in timestamp order
        let priority_queue = self.base.priority_queue.clone();
        let reaction_name = self.base.config.id.clone();
        let app_tx = self.app_tx.clone();
        let query_filter = self.base.config.queries.clone();

        let processing_task = tokio::spawn(async move {
            info!(
                "ApplicationReaction '{}' result processor started",
                reaction_name
            );

            loop {
                // Dequeue next result in timestamp order (blocking)
                let query_result_arc = priority_queue.dequeue().await;

                // Clone to get owned QueryResult
                let query_result = (*query_result_arc).clone();

                // Filter results based on configured queries
                if !query_filter.is_empty() && !query_filter.contains(&query_result.query_id) {
                    continue;
                }

                debug!(
                    "ApplicationReaction '{}' forwarding result from query '{}': {} results",
                    reaction_name,
                    query_result.query_id,
                    query_result.results.len()
                );

                // Forward to application
                if let Err(e) = app_tx.send(query_result).await {
                    error!("Failed to send result to application: {}", e);
                    break;
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
                Some("Application reaction stopped".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    fn get_config(&self) -> &ReactionConfig {
        &self.base.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn create_test_reaction_config(id: &str, queries: Vec<String>) -> ReactionConfig {
        use crate::config::typed::ApplicationReactionConfig;

        ReactionConfig {
            id: id.to_string(),
            reaction_type: "application".to_string(),
            auto_start: true,
            queries,
            config: crate::config::ReactionSpecificConfig::Application(ApplicationReactionConfig {
                properties: HashMap::new(),
            }),
            priority_queue_capacity: None,
        }
    }

    fn create_test_query_result(query_id: &str) -> QueryResult {
        QueryResult {
            query_id: query_id.to_string(),
            results: vec![],
            metadata: HashMap::new(),
            profiling: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_application_reaction_creation() {
        let config = create_test_reaction_config("test-reaction", vec![]);
        let (event_tx, _) = mpsc::channel(100);

        let (_reaction, handle) = ApplicationReaction::new(config, event_tx);

        assert_eq!(handle.reaction_id(), "test-reaction");
    }

    #[tokio::test]
    async fn test_handle_take_receiver() {
        let config = create_test_reaction_config("test-reaction", vec![]);
        let (event_tx, _) = mpsc::channel(100);

        let (_reaction, handle) = ApplicationReaction::new(config, event_tx);

        let receiver = handle.take_receiver().await;
        assert!(receiver.is_some());

        // Second call should return None
        let receiver2 = handle.take_receiver().await;
        assert!(receiver2.is_none());
    }

    #[tokio::test]
    async fn test_handle_subscribe() {
        let config = create_test_reaction_config("test-reaction", vec![]);
        let (event_tx, _) = mpsc::channel(100);

        let (_reaction, handle) = ApplicationReaction::new(config, event_tx);

        let result = handle
            .subscribe(|_result| {
                // Callback logic
            })
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_subscribe_fails_after_take() {
        let config = create_test_reaction_config("test-reaction", vec![]);
        let (event_tx, _) = mpsc::channel(100);

        let (_reaction, handle) = ApplicationReaction::new(config, event_tx);

        // Take receiver first
        let _receiver = handle.take_receiver().await;

        // Subscribe should fail
        let result = handle.subscribe(|_result| {}).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_handle_subscribe_filtered() {
        let config = create_test_reaction_config("test-reaction", vec![]);
        let (event_tx, _) = mpsc::channel(100);

        let (_reaction, handle) = ApplicationReaction::new(config, event_tx);

        let result = handle
            .subscribe_filtered(vec!["query-1".to_string()], |_result| {
                // Callback logic
            })
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_as_stream() {
        let config = create_test_reaction_config("test-reaction", vec![]);
        let (event_tx, _) = mpsc::channel(100);

        let (_reaction, handle) = ApplicationReaction::new(config, event_tx);

        let stream = handle.as_stream().await;
        assert!(stream.is_some());

        // Second call should return None
        let stream2 = handle.as_stream().await;
        assert!(stream2.is_none());
    }

    #[tokio::test]
    async fn test_handle_subscribe_with_options() {
        let config = create_test_reaction_config("test-reaction", vec![]);
        let (event_tx, _) = mpsc::channel(100);

        let (_reaction, handle) = ApplicationReaction::new(config, event_tx);

        let options = SubscriptionOptions::default();
        let result = handle.subscribe_with_options(options).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_result_stream_try_next() {
        let config = create_test_reaction_config("test-reaction", vec![]);
        let (event_tx, _) = mpsc::channel(100);

        let (_reaction, handle) = ApplicationReaction::new(config, event_tx);

        let mut stream = handle.as_stream().await.unwrap();

        // Should return None when no results
        let result = stream.try_next();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_handle_clone() {
        let config = create_test_reaction_config("test-reaction", vec![]);
        let (event_tx, _) = mpsc::channel(100);

        let (_reaction, handle) = ApplicationReaction::new(config, event_tx);

        let handle2 = handle.clone();
        assert_eq!(handle.reaction_id(), handle2.reaction_id());
    }

    #[tokio::test]
    async fn test_reaction_initial_status() {
        let config = create_test_reaction_config("test-reaction", vec![]);
        let (event_tx, _) = mpsc::channel(100);

        let (reaction, _handle) = ApplicationReaction::new(config, event_tx);

        let status = reaction.status().await;
        assert!(matches!(status, ComponentStatus::Stopped));
    }

    #[tokio::test]
    async fn test_reaction_config_access() {
        let config = create_test_reaction_config("test-reaction", vec!["query-1".to_string()]);
        let (event_tx, _) = mpsc::channel(100);

        let (reaction, _handle) = ApplicationReaction::new(config, event_tx);

        let config = reaction.get_config();
        assert_eq!(config.id, "test-reaction");
        assert_eq!(config.queries.len(), 1);
        assert_eq!(config.queries[0], "query-1");
    }

    #[tokio::test]
    async fn test_reaction_with_multiple_queries() {
        let config = create_test_reaction_config(
            "test-reaction",
            vec!["query-1".to_string(), "query-2".to_string()],
        );
        let (event_tx, _) = mpsc::channel(100);

        let (reaction, _handle) = ApplicationReaction::new(config, event_tx);

        let config = reaction.get_config();
        assert_eq!(config.queries.len(), 2);
    }

    #[tokio::test]
    async fn test_reaction_with_empty_queries() {
        let config = create_test_reaction_config("test-reaction", vec![]);
        let (event_tx, _) = mpsc::channel(100);

        let (reaction, _handle) = ApplicationReaction::new(config, event_tx);

        let config = reaction.get_config();
        assert!(config.queries.is_empty());
    }

    #[tokio::test]
    async fn test_multiple_reactions_independent() {
        let config1 = create_test_reaction_config("reaction-1", vec![]);
        let config2 = create_test_reaction_config("reaction-2", vec![]);
        let (event_tx1, _) = mpsc::channel(100);
        let (event_tx2, _) = mpsc::channel(100);

        let (_reaction1, handle1) = ApplicationReaction::new(config1, event_tx1);
        let (_reaction2, handle2) = ApplicationReaction::new(config2, event_tx2);

        assert_eq!(handle1.reaction_id(), "reaction-1");
        assert_eq!(handle2.reaction_id(), "reaction-2");
    }

    #[tokio::test]
    async fn test_reaction_creation_with_config() {
        let config = create_test_reaction_config("test-reaction", vec![]);

        let (event_tx, _) = mpsc::channel(100);
        let (reaction, _handle) = ApplicationReaction::new(config, event_tx);

        let config = reaction.get_config();
        assert_eq!(config.id, "test-reaction");
        // Verify we can get properties through the helper method
        let props = config.get_properties();
        assert!(props.is_empty());
    }

    #[tokio::test]
    async fn test_subscription_options_default() {
        let options = SubscriptionOptions::default();

        // Just verify we can create default options
        let config = create_test_reaction_config("test-reaction", vec![]);
        let (event_tx, _) = mpsc::channel(100);
        let (_reaction, handle) = ApplicationReaction::new(config, event_tx);

        let result = handle.subscribe_with_options(options).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_query_result_creation_helper() {
        let result = create_test_query_result("test-query");

        assert_eq!(result.query_id, "test-query");
        assert!(result.results.is_empty());
        assert!(result.metadata.is_empty());
    }
}
