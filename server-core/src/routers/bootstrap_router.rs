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

use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapProviderConfig, BootstrapProviderFactory,
};
use crate::channels::{
    BootstrapRequestReceiver, BootstrapResponseSender, ControlSignal, ControlSignalSender,
    ControlSignalWrapper, SourceEventSender
};
use crate::config::SourceConfig;

/// Routes bootstrap requests from queries to providers and manages bootstrap state
pub struct BootstrapRouter {
    /// Map of source name to the bootstrap provider instance
    providers: Arc<RwLock<HashMap<String, Box<dyn BootstrapProvider>>>>,
    /// Map of source name to bootstrap context for providers
    contexts: Arc<RwLock<HashMap<String, BootstrapContext>>>,
    /// Map of query name to its bootstrap response sender
    query_response_senders: Arc<RwLock<HashMap<String, BootstrapResponseSender>>>,
    /// Track bootstrap state for each query-source pair
    bootstrap_state: Arc<RwLock<HashMap<(String, String), BootstrapState>>>,
    /// Control signal sender for bootstrap coordination
    control_signal_tx: Option<ControlSignalSender>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum BootstrapState {
    Pending,
    InProgress,
    Completed { element_count: usize },
    Failed { error: String },
}

impl Default for BootstrapRouter {
    fn default() -> Self {
        Self {
            providers: Arc::new(RwLock::new(HashMap::new())),
            contexts: Arc::new(RwLock::new(HashMap::new())),
            query_response_senders: Arc::new(RwLock::new(HashMap::new())),
            bootstrap_state: Arc::new(RwLock::new(HashMap::new())),
            control_signal_tx: None,
        }
    }
}

impl BootstrapRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the control signal sender for bootstrap coordination
    pub fn set_control_signal_sender(&mut self, tx: ControlSignalSender) {
        self.control_signal_tx = Some(tx);
    }

    /// Register a bootstrap provider for a source
    pub async fn register_provider(
        &self,
        server_id: String,
        source_config: Arc<SourceConfig>,
        provider_config: Option<BootstrapProviderConfig>,
        source_event_tx: SourceEventSender,
    ) -> anyhow::Result<()> {
        let source_id = source_config.id.clone();

        // Create the bootstrap context
        let context = BootstrapContext::new(
            server_id,
            source_config.clone(),
            source_event_tx,
            source_id.clone(),
        );

        // Create the provider (or use no-op if no config provided)
        let provider = match provider_config {
            Some(config) => BootstrapProviderFactory::create_provider(&config)?,
            None => BootstrapProviderFactory::create_provider(&BootstrapProviderConfig::Noop)?,
        };

        // Store the provider and context
        self.providers
            .write()
            .await
            .insert(source_id.clone(), provider);
        self.contexts
            .write()
            .await
            .insert(source_id.clone(), context);

        info!("Registered bootstrap provider for source '{}'", source_id);
        Ok(())
    }

    /// Remove a provider from the bootstrap router
    #[allow(dead_code)]
    pub async fn unregister_provider(&self, source_id: &str) {
        self.providers.write().await.remove(source_id);
        self.contexts.write().await.remove(source_id);

        // Clear any bootstrap state for this source
        let mut states = self.bootstrap_state.write().await;
        states.retain(|(_, source_name), _| source_name != source_id);

        info!("Unregistered bootstrap provider for source '{}'", source_id);
    }

    /// Register a query's response channel
    pub async fn register_query(&self, query_id: String, response_tx: BootstrapResponseSender) {
        self.query_response_senders
            .write()
            .await
            .insert(query_id.clone(), response_tx);
        info!("Registered query '{}' for bootstrap responses", query_id);
    }

    /// Unregister a query
    #[allow(dead_code)]
    pub async fn unregister_query(&self, query_id: &str) {
        self.query_response_senders.write().await.remove(query_id);

        // Clear any bootstrap state for this query
        let mut states = self.bootstrap_state.write().await;
        states.retain(|(query, _), _| query != query_id);

        debug!("Unregistered query '{}' from bootstrap", query_id);
    }

    /// Start the bootstrap router
    pub async fn start(&self, mut bootstrap_request_rx: BootstrapRequestReceiver) {
        info!("Starting bootstrap router");

        while let Some(request) = bootstrap_request_rx.recv().await {
            let query_id = request.query_id.clone();
            let request_id = request.request_id.clone();

            debug!(
                "Bootstrap router received request from query '{}' (id: {})",
                query_id, request_id
            );

            // Process each provider that the query needs to bootstrap from
            // Note: This assumes the query knows which sources it needs.
            // In a real implementation, you might want to track query-source subscriptions

            let providers = self.providers.read().await;
            let contexts = self.contexts.read().await;
            let response_senders = self.query_response_senders.read().await;

            if let Some(response_tx) = response_senders.get(&query_id) {
                // Send initial acknowledgment
                let ack_response = crate::channels::BootstrapResponse {
                    request_id: request_id.clone(),
                    status: crate::channels::BootstrapStatus::Started,
                    message: Some("Bootstrap request received".to_string()),
                };

                if let Err(e) = response_tx.send(ack_response).await {
                    error!(
                        "Failed to send bootstrap acknowledgment to query '{}': {}",
                        query_id, e
                    );
                    continue;
                }

                // For now, we'll bootstrap from all providers
                // In a real implementation, you'd only bootstrap from sources the query subscribes to
                for (source_id, provider) in providers.iter() {
                    if let Some(context) = contexts.get(source_id) {
                        let state_key = (query_id.clone(), source_id.clone());

                        // Update state to in-progress
                        self.bootstrap_state
                            .write()
                            .await
                            .insert(state_key.clone(), BootstrapState::InProgress);

                        // Send control signal for bootstrap start
                        if let Some(ref control_tx) = self.control_signal_tx {
                            let start_signal = ControlSignalWrapper::new(
                                ControlSignal::BootstrapStarted {
                                    query_id: query_id.clone(),
                                    source_id: source_id.clone(),
                                }
                            );
                            if let Err(e) = control_tx.send(start_signal).await {
                                error!(
                                    "Failed to send bootstrap started control signal for query '{}' from source '{}': {}",
                                    query_id, source_id, e
                                );
                            } else {
                                info!(
                                    "Sent bootstrap started control signal for query '{}' from source '{}'",
                                    query_id, source_id
                                );
                            }
                        }

                        // Send BootstrapStart marker before bootstrap begins (for backward compatibility)
                        let start_marker = crate::channels::SourceEventWrapper::new(
                            source_id.clone(),
                            crate::channels::SourceEvent::BootstrapStart {
                                query_id: query_id.clone(),
                            },
                            chrono::Utc::now(),
                        );

                        if let Err(e) = context.source_event_tx.send(start_marker).await {
                            error!(
                                "Failed to send bootstrap start marker for query '{}' from source '{}': {}",
                                query_id, source_id, e
                            );
                            continue;
                        }

                        info!(
                            "Sent bootstrap start marker for query '{}' from source '{}'",
                            query_id, source_id
                        );

                        // Execute bootstrap
                        match provider.bootstrap(request.clone(), context).await {
                            Ok(element_count) => {
                                info!(
                                "Bootstrap completed for query '{}' from source '{}': {} elements",
                                query_id, source_id, element_count
                            );

                                // Send control signal for bootstrap completion
                                if let Some(ref control_tx) = self.control_signal_tx {
                                    let complete_signal = ControlSignalWrapper::new(
                                        ControlSignal::BootstrapCompleted {
                                            query_id: query_id.clone(),
                                            source_id: source_id.clone(),
                                        }
                                    );
                                    if let Err(e) = control_tx.send(complete_signal).await {
                                        error!(
                                            "Failed to send bootstrap completed control signal for query '{}' from source '{}': {}",
                                            query_id, source_id, e
                                        );
                                    } else {
                                        info!(
                                            "Sent bootstrap completed control signal for query '{}' from source '{}'",
                                            query_id, source_id
                                        );
                                    }
                                }

                                // Send BootstrapEnd marker after bootstrap completes (for backward compatibility)
                                let end_marker = crate::channels::SourceEventWrapper::new(
                                    source_id.clone(),
                                    crate::channels::SourceEvent::BootstrapEnd {
                                        query_id: query_id.clone(),
                                    },
                                    chrono::Utc::now(),
                                );

                                if let Err(e) = context.source_event_tx.send(end_marker).await {
                                    error!(
                                        "Failed to send bootstrap end marker for query '{}' from source '{}': {}",
                                        query_id, source_id, e
                                    );
                                } else {
                                    info!(
                                        "Sent bootstrap end marker for query '{}' from source '{}'",
                                        query_id, source_id
                                    );
                                }

                                // Update state to completed
                                self.bootstrap_state
                                    .write()
                                    .await
                                    .insert(state_key, BootstrapState::Completed { element_count });

                                // Send completion response
                                let complete_response = crate::channels::BootstrapResponse {
                                    request_id: request_id.clone(),
                                    status: crate::channels::BootstrapStatus::Completed {
                                        total_count: element_count,
                                    },
                                    message: Some(format!(
                                        "Bootstrap from source '{}' completed",
                                        source_id
                                    )),
                                };

                                if let Err(e) = response_tx.send(complete_response).await {
                                    error!(
                                        "Failed to send bootstrap completion to query '{}': {}",
                                        query_id, e
                                    );
                                }
                            }
                            Err(e) => {
                                error!(
                                    "Bootstrap failed for query '{}' from source '{}': {}",
                                    query_id, source_id, e
                                );

                                // Update state to failed
                                self.bootstrap_state.write().await.insert(
                                    state_key,
                                    BootstrapState::Failed {
                                        error: e.to_string(),
                                    },
                                );

                                // Send failure response
                                let fail_response = crate::channels::BootstrapResponse {
                                    request_id: request_id.clone(),
                                    status: crate::channels::BootstrapStatus::Failed {
                                        error: e.to_string(),
                                    },
                                    message: Some(format!(
                                        "Bootstrap from source '{}' failed",
                                        source_id
                                    )),
                                };

                                if let Err(e) = response_tx.send(fail_response).await {
                                    error!(
                                        "Failed to send bootstrap failure to query '{}': {}",
                                        query_id, e
                                    );
                                }
                            }
                        }
                    }
                }
            } else {
                warn!("No response channel found for query '{}'", query_id);
            }
        }

        info!("Bootstrap router stopped");
    }

    /// Get the bootstrap state for a query-source pair
    #[allow(dead_code)]
    #[allow(private_interfaces)]
    pub async fn get_bootstrap_state(
        &self,
        query_id: &str,
        source_name: &str,
    ) -> Option<BootstrapState> {
        let states = self.bootstrap_state.read().await;
        states
            .get(&(query_id.to_string(), source_name.to_string()))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::BootstrapProviderConfig;
    use crate::channels::{BootstrapRequest, BootstrapStatus};
    use crate::config::SourceConfig;
    use tokio::sync::mpsc;

    fn create_test_source_config(id: &str) -> Arc<SourceConfig> {
        Arc::new(SourceConfig {
            id: id.to_string(),
            source_type: "mock".to_string(),
            auto_start: true,
            properties: std::collections::HashMap::new(),
            bootstrap_provider: None,
        })
    }

    #[tokio::test]
    async fn test_bootstrap_router_creation() {
        let router = BootstrapRouter::new();

        // Verify empty initial state
        assert!(router.providers.read().await.is_empty());
        assert!(router.contexts.read().await.is_empty());
        assert!(router.query_response_senders.read().await.is_empty());
        assert!(router.bootstrap_state.read().await.is_empty());
    }

    #[tokio::test]
    async fn test_register_provider() {
        let router = BootstrapRouter::new();
        let source_config = create_test_source_config("test-source");
        let (source_event_tx, _) = mpsc::channel(100);

        let result = router
            .register_provider(
                "test-server".to_string(),
                source_config.clone(),
                Some(BootstrapProviderConfig::Noop),
                source_event_tx,
            )
            .await;

        assert!(result.is_ok());

        // Verify provider and context are registered
        let providers = router.providers.read().await;
        let contexts = router.contexts.read().await;
        assert!(providers.contains_key("test-source"));
        assert!(contexts.contains_key("test-source"));
    }

    #[tokio::test]
    async fn test_register_provider_with_no_config() {
        let router = BootstrapRouter::new();
        let source_config = create_test_source_config("test-source");
        let (source_event_tx, _) = mpsc::channel(100);

        let result = router
            .register_provider(
                "test-server".to_string(),
                source_config.clone(),
                None, // No bootstrap config - should use Noop provider
                source_event_tx,
            )
            .await;

        assert!(result.is_ok());
        assert!(router.providers.read().await.contains_key("test-source"));
    }

    #[tokio::test]
    async fn test_unregister_provider() {
        let router = BootstrapRouter::new();
        let source_config = create_test_source_config("test-source");
        let (source_event_tx, _) = mpsc::channel(100);

        router
            .register_provider(
                "test-server".to_string(),
                source_config.clone(),
                Some(BootstrapProviderConfig::Noop),
                source_event_tx,
            )
            .await
            .unwrap();

        // Verify registered
        assert!(router.providers.read().await.contains_key("test-source"));

        // Unregister
        router.unregister_provider("test-source").await;

        // Verify removed
        assert!(!router.providers.read().await.contains_key("test-source"));
        assert!(!router.contexts.read().await.contains_key("test-source"));
    }

    #[tokio::test]
    async fn test_register_query() {
        let router = BootstrapRouter::new();
        let (response_tx, _response_rx) = mpsc::channel(100);

        router
            .register_query("test-query".to_string(), response_tx)
            .await;

        // Verify query is registered
        let senders = router.query_response_senders.read().await;
        assert!(senders.contains_key("test-query"));
    }

    #[tokio::test]
    async fn test_unregister_query() {
        let router = BootstrapRouter::new();
        let (response_tx, _response_rx) = mpsc::channel(100);

        router
            .register_query("test-query".to_string(), response_tx)
            .await;

        // Verify registered
        assert!(router
            .query_response_senders
            .read()
            .await
            .contains_key("test-query"));

        // Unregister
        router.unregister_query("test-query").await;

        // Verify removed
        assert!(!router
            .query_response_senders
            .read()
            .await
            .contains_key("test-query"));
    }

    #[tokio::test]
    async fn test_register_multiple_providers() {
        let router = BootstrapRouter::new();
        let (source_event_tx, _) = mpsc::channel(100);

        for i in 1..=3 {
            let source_config = create_test_source_config(&format!("source-{}", i));
            router
                .register_provider(
                    "test-server".to_string(),
                    source_config,
                    Some(BootstrapProviderConfig::Noop),
                    source_event_tx.clone(),
                )
                .await
                .unwrap();
        }

        // Verify all providers are registered
        let providers = router.providers.read().await;
        assert_eq!(providers.len(), 3);
        assert!(providers.contains_key("source-1"));
        assert!(providers.contains_key("source-2"));
        assert!(providers.contains_key("source-3"));
    }

    #[tokio::test]
    async fn test_register_multiple_queries() {
        let router = BootstrapRouter::new();

        for i in 1..=3 {
            let (response_tx, _) = mpsc::channel(100);
            router
                .register_query(format!("query-{}", i), response_tx)
                .await;
        }

        // Verify all queries are registered
        let senders = router.query_response_senders.read().await;
        assert_eq!(senders.len(), 3);
        assert!(senders.contains_key("query-1"));
        assert!(senders.contains_key("query-2"));
        assert!(senders.contains_key("query-3"));
    }

    #[tokio::test]
    async fn test_unregister_provider_clears_bootstrap_state() {
        let router = BootstrapRouter::new();
        let source_config = create_test_source_config("test-source");
        let (source_event_tx, _) = mpsc::channel(100);

        router
            .register_provider(
                "test-server".to_string(),
                source_config.clone(),
                Some(BootstrapProviderConfig::Noop),
                source_event_tx,
            )
            .await
            .unwrap();

        // Simulate bootstrap state
        router.bootstrap_state.write().await.insert(
            ("query-1".to_string(), "test-source".to_string()),
            BootstrapState::Completed { element_count: 100 },
        );

        // Unregister provider
        router.unregister_provider("test-source").await;

        // Verify bootstrap state is cleared
        let states = router.bootstrap_state.read().await;
        assert!(!states.contains_key(&("query-1".to_string(), "test-source".to_string())));
    }

    #[tokio::test]
    async fn test_unregister_query_clears_bootstrap_state() {
        let router = BootstrapRouter::new();
        let (response_tx, _) = mpsc::channel(100);

        router
            .register_query("test-query".to_string(), response_tx)
            .await;

        // Simulate bootstrap state
        router.bootstrap_state.write().await.insert(
            ("test-query".to_string(), "source-1".to_string()),
            BootstrapState::Completed { element_count: 50 },
        );

        // Unregister query
        router.unregister_query("test-query").await;

        // Verify bootstrap state is cleared
        let states = router.bootstrap_state.read().await;
        assert!(!states.contains_key(&("test-query".to_string(), "source-1".to_string())));
    }

    #[tokio::test]
    async fn test_get_bootstrap_state_for_pending() {
        let router = BootstrapRouter::new();

        router.bootstrap_state.write().await.insert(
            ("query-1".to_string(), "source-1".to_string()),
            BootstrapState::Pending,
        );

        let state = router.get_bootstrap_state("query-1", "source-1").await;
        assert!(state.is_some());
        assert!(matches!(state.unwrap(), BootstrapState::Pending));
    }

    #[tokio::test]
    async fn test_get_bootstrap_state_for_nonexistent() {
        let router = BootstrapRouter::new();

        let state = router.get_bootstrap_state("nonexistent", "source-1").await;
        assert!(state.is_none());
    }

    #[tokio::test]
    async fn test_bootstrap_state_transitions() {
        let router = BootstrapRouter::new();
        let key = ("query-1".to_string(), "source-1".to_string());

        // Pending state
        router
            .bootstrap_state
            .write()
            .await
            .insert(key.clone(), BootstrapState::Pending);
        let state = router.get_bootstrap_state("query-1", "source-1").await;
        assert!(matches!(state.unwrap(), BootstrapState::Pending));

        // InProgress state
        router
            .bootstrap_state
            .write()
            .await
            .insert(key.clone(), BootstrapState::InProgress);
        let state = router.get_bootstrap_state("query-1", "source-1").await;
        assert!(matches!(state.unwrap(), BootstrapState::InProgress));

        // Completed state
        router.bootstrap_state.write().await.insert(
            key.clone(),
            BootstrapState::Completed { element_count: 100 },
        );
        let state = router.get_bootstrap_state("query-1", "source-1").await;
        assert!(matches!(state.unwrap(), BootstrapState::Completed { .. }));

        // Failed state
        router.bootstrap_state.write().await.insert(
            key.clone(),
            BootstrapState::Failed {
                error: "test error".to_string(),
            },
        );
        let state = router.get_bootstrap_state("query-1", "source-1").await;
        assert!(matches!(state.unwrap(), BootstrapState::Failed { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bootstrap_router_handles_request_with_no_providers() {
        let router = Arc::new(BootstrapRouter::new());
        let (request_tx, request_rx) = mpsc::channel(100);
        let (response_tx, mut response_rx) = mpsc::channel(100);

        // Register query but no providers
        router
            .register_query("test-query".to_string(), response_tx)
            .await;

        // Start router in background
        let router_clone = router.clone();
        let router_task = tokio::spawn(async move {
            router_clone.start(request_rx).await;
        });

        // Send bootstrap request
        let request = BootstrapRequest {
            query_id: "test-query".to_string(),
            node_labels: vec!["Person".to_string()],
            relation_labels: vec![],
            request_id: "req-1".to_string(),
        };

        request_tx.send(request).await.unwrap();

        // Should receive Started acknowledgment (no providers to bootstrap from)
        let response =
            tokio::time::timeout(std::time::Duration::from_millis(500), response_rx.recv()).await;

        assert!(response.is_ok());
        let response = response.unwrap().unwrap();
        assert_eq!(response.request_id, "req-1");
        assert!(matches!(response.status, BootstrapStatus::Started));

        // Clean up
        drop(request_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), router_task).await;
    }

    #[tokio::test]
    async fn test_bootstrap_router_default_initialization() {
        let router = BootstrapRouter::default();

        // Verify all collections are empty
        assert!(router.providers.read().await.is_empty());
        assert!(router.contexts.read().await.is_empty());
        assert!(router.query_response_senders.read().await.is_empty());
        assert!(router.bootstrap_state.read().await.is_empty());
    }

    #[tokio::test]
    async fn test_bootstrap_state_enum_variants() {
        // Test all BootstrapState variants can be created
        let _pending = BootstrapState::Pending;
        let _in_progress = BootstrapState::InProgress;
        let _completed = BootstrapState::Completed { element_count: 42 };
        let _failed = BootstrapState::Failed {
            error: "test error".to_string(),
        };
    }

    #[tokio::test]
    async fn test_register_provider_overwrites_existing() {
        let router = BootstrapRouter::new();
        let source_config = create_test_source_config("test-source");
        let (source_event_tx1, _) = mpsc::channel(100);
        let (source_event_tx2, _) = mpsc::channel(100);

        // Register first provider
        router
            .register_provider(
                "server-1".to_string(),
                source_config.clone(),
                Some(BootstrapProviderConfig::Noop),
                source_event_tx1,
            )
            .await
            .unwrap();

        // Register second provider with same source ID
        router
            .register_provider(
                "server-2".to_string(),
                source_config.clone(),
                Some(BootstrapProviderConfig::Noop),
                source_event_tx2,
            )
            .await
            .unwrap();

        // Should still have only one provider (overwritten)
        let providers = router.providers.read().await;
        assert_eq!(providers.len(), 1);
        assert!(providers.contains_key("test-source"));
    }

    #[tokio::test]
    async fn test_unregister_nonexistent_provider() {
        let router = BootstrapRouter::new();

        // Should not panic when unregistering nonexistent provider
        router.unregister_provider("nonexistent").await;

        // State should remain empty
        assert!(router.providers.read().await.is_empty());
    }

    #[tokio::test]
    async fn test_unregister_nonexistent_query() {
        let router = BootstrapRouter::new();

        // Should not panic when unregistering nonexistent query
        router.unregister_query("nonexistent").await;

        // State should remain empty
        assert!(router.query_response_senders.read().await.is_empty());
    }
}
