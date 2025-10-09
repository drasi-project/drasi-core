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
use crate::channels::{BootstrapRequestReceiver, BootstrapResponseSender, SourceChangeSender};
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
        }
    }
}

impl BootstrapRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a bootstrap provider for a source
    pub async fn register_provider(
        &self,
        source_config: Arc<SourceConfig>,
        provider_config: Option<BootstrapProviderConfig>,
        source_change_tx: SourceChangeSender,
    ) -> anyhow::Result<()> {
        let source_id = source_config.id.clone();

        // Create the bootstrap context
        let context =
            BootstrapContext::new(source_config.clone(), source_change_tx, source_id.clone());

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

                        // Execute bootstrap
                        match provider.bootstrap(request.clone(), context).await {
                            Ok(element_count) => {
                                info!(
                                "Bootstrap completed for query '{}' from source '{}': {} elements",
                                query_id, source_id, element_count
                            );

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
