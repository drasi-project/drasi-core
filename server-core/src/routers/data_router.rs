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
use tokio::sync::{mpsc, RwLock};

use crate::channels::{SourceChangeReceiver, SourceChangeSender, SourceEventReceiver, SourceEventSender, SourceEvent};

/// Routes data changes from sources to subscribed queries
pub struct DataRouter {
    /// Map of query id to its sender channel (legacy)
    query_senders: Arc<RwLock<HashMap<String, SourceChangeSender>>>,
    /// Map of query id to its unified event sender channel
    query_event_senders: Arc<RwLock<HashMap<String, SourceEventSender>>>,
    /// Map of source id to list of query ids that subscribe to it
    source_subscriptions: Arc<RwLock<HashMap<String, Vec<String>>>>,
}

impl Default for DataRouter {
    fn default() -> Self {
        Self {
            query_senders: Arc::new(RwLock::new(HashMap::new())),
            query_event_senders: Arc::new(RwLock::new(HashMap::new())),
            source_subscriptions: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl DataRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a query subscription to specific sources
    pub async fn add_query_subscription(
        &self,
        query_id: String,
        source_ids: Vec<String>,
    ) -> SourceChangeReceiver {
        let start_time = std::time::Instant::now();
        info!(
            "Starting to add subscription for query '{}' to sources: {:?}",
            query_id, source_ids
        );

        // Check if query already has a subscription
        let existing_sender = self.query_senders.read().await.contains_key(&query_id);
        if existing_sender {
            warn!("Query '{}' already has a subscription. Creating new subscription will disconnect the old one.", query_id);
        }

        let (tx, rx) = mpsc::channel(1000);

        // Store the sender for this query (overwrites existing if any)
        self.query_senders
            .write()
            .await
            .insert(query_id.clone(), tx);
        info!("Channel created and sender stored for query '{}'", query_id);

        // Update source subscriptions
        let mut subscriptions = self.source_subscriptions.write().await;
        for source_id in &source_ids {
            let query_list = subscriptions
                .entry(source_id.clone())
                .or_insert_with(Vec::new);

            // Only add if not already subscribed
            if !query_list.contains(&query_id) {
                query_list.push(query_id.clone());
                info!(
                    "Query '{}' successfully subscribed to source '{}'",
                    query_id, source_id
                );
            } else {
                debug!(
                    "Query '{}' already subscribed to source '{}'",
                    query_id, source_id
                );
            }
        }

        let elapsed = start_time.elapsed();
        info!(
            "Completed subscription for query '{}' to {} sources in {:?}",
            query_id,
            source_ids.len(),
            elapsed
        );

        rx
    }

    /// Remove a query subscription
    pub async fn remove_query_subscription(&self, query_id: &str) {
        // Remove the sender
        self.query_senders.write().await.remove(query_id);

        // Remove from all source subscriptions
        let mut subscriptions = self.source_subscriptions.write().await;
        for queries in subscriptions.values_mut() {
            queries.retain(|id| id != query_id);
        }

        info!("Removed subscription for query '{}'", query_id);
    }

    /// Add a query subscription to specific sources (unified events)
    pub async fn add_query_event_subscription(
        &self,
        query_id: String,
        source_ids: Vec<String>,
    ) -> SourceEventReceiver {
        let start_time = std::time::Instant::now();
        info!(
            "Starting to add unified event subscription for query '{}' to sources: {:?}",
            query_id, source_ids
        );

        // Check if query already has a subscription
        let existing_sender = self.query_event_senders.read().await.contains_key(&query_id);
        if existing_sender {
            warn!("Query '{}' already has a unified event subscription. Creating new subscription will disconnect the old one.", query_id);
        }

        let (tx, rx) = mpsc::channel(1000);

        // Store the sender for this query (overwrites existing if any)
        self.query_event_senders
            .write()
            .await
            .insert(query_id.clone(), tx);
        info!("Unified event channel created and sender stored for query '{}'", query_id);

        // Update source subscriptions
        let mut subscriptions = self.source_subscriptions.write().await;
        for source_id in &source_ids {
            let query_list = subscriptions
                .entry(source_id.clone())
                .or_insert_with(Vec::new);

            // Only add if not already subscribed
            if !query_list.contains(&query_id) {
                query_list.push(query_id.clone());
                info!(
                    "Query '{}' successfully subscribed to source '{}' (unified events)",
                    query_id, source_id
                );
            } else {
                debug!(
                    "Query '{}' already subscribed to source '{}'",
                    query_id, source_id
                );
            }
        }

        let elapsed = start_time.elapsed();
        info!(
            "Completed unified event subscription for query '{}' to {} sources in {:?}",
            query_id,
            source_ids.len(),
            elapsed
        );

        rx
    }

    /// Start the router that receives source changes and forwards them to subscribed queries
    pub async fn start(&self, mut source_change_rx: SourceChangeReceiver) {
        info!("Starting data router");

        while let Some(source_event) = source_change_rx.recv().await {
            let source_id = &source_event.source_id;
            debug!("Data router received change from source '{}'", source_id);

            // Get list of queries subscribed to this source
            let subscriptions = self.source_subscriptions.read().await;
            if let Some(query_ids) = subscriptions.get(source_id) {
                debug!(
                    "[DATA-FLOW] Source '{}' has {} subscribed queries",
                    source_id,
                    query_ids.len()
                );
                let senders = self.query_senders.read().await;

                for query_id in query_ids {
                    if let Some(sender) = senders.get(query_id) {
                        debug!(
                            "[DATA-FLOW] Sending change from source '{}' to query '{}'",
                            source_id, query_id
                        );
                        // Clone the source event for each query
                        match sender.send(source_event.clone()).await {
                            Ok(_) => {
                                debug!(
                                    "Routed change from source '{}' to query '{}'",
                                    source_id, query_id
                                );
                            }
                            Err(e) => {
                                error!("Failed to send change to query '{}': {}", query_id, e);
                            }
                        }
                    }
                }
            } else {
                debug!("No queries subscribed to source '{}'", source_id);
            }
        }

        info!("Data router stopped");
    }

    /// Start the router for unified events (SourceEvent)
    pub async fn start_unified(&self, mut source_event_rx: SourceEventReceiver) {
        info!("Starting data router (unified events)");

        while let Some(source_event_wrapper) = source_event_rx.recv().await {
            let source_id = &source_event_wrapper.source_id;

            // Log event type for visibility
            match &source_event_wrapper.event {
                SourceEvent::Change(_) => {
                    debug!("Data router received Change event from source '{}'", source_id);
                }
                SourceEvent::Control(_) => {
                    debug!("Data router received Control event from source '{}'", source_id);
                }
            }

            // Get list of queries subscribed to this source
            let subscriptions = self.source_subscriptions.read().await;
            if let Some(query_ids) = subscriptions.get(source_id) {
                debug!(
                    "[DATA-FLOW] Source '{}' has {} subscribed queries",
                    source_id,
                    query_ids.len()
                );
                let senders = self.query_event_senders.read().await;

                for query_id in query_ids {
                    if let Some(sender) = senders.get(query_id) {
                        debug!(
                            "[DATA-FLOW] Sending event from source '{}' to query '{}'",
                            source_id, query_id
                        );
                        // Clone the source event wrapper for each query
                        match sender.send(source_event_wrapper.clone()).await {
                            Ok(_) => {
                                debug!(
                                    "Routed event from source '{}' to query '{}'",
                                    source_id, query_id
                                );
                            }
                            Err(e) => {
                                error!("Failed to send event to query '{}': {}", query_id, e);
                            }
                        }
                    }
                }
            } else {
                debug!("No queries subscribed to source '{}'", source_id);
            }
        }

        info!("Data router stopped (unified events)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_new_router() {
        let router = DataRouter::new();

        // Verify initial state
        let senders = router.query_senders.read().await;
        let subscriptions = router.source_subscriptions.read().await;

        assert!(senders.is_empty());
        assert!(subscriptions.is_empty());
    }

    #[tokio::test]
    async fn test_add_single_subscription() {
        let router = DataRouter::new();

        let rx = router
            .add_query_subscription("query1".to_string(), vec!["source1".to_string()])
            .await;

        // Verify query sender is stored
        let senders = router.query_senders.read().await;
        assert!(senders.contains_key("query1"));

        // Verify source subscription is recorded
        let subscriptions = router.source_subscriptions.read().await;
        assert_eq!(
            subscriptions.get("source1"),
            Some(&vec!["query1".to_string()])
        );

        // Verify receiver is functional
        assert!(!rx.is_closed());
    }

    #[tokio::test]
    async fn test_multiple_queries_same_source() {
        let router = DataRouter::new();

        let _rx1 = router
            .add_query_subscription("query1".to_string(), vec!["source1".to_string()])
            .await;

        let _rx2 = router
            .add_query_subscription("query2".to_string(), vec!["source1".to_string()])
            .await;

        let subscriptions = router.source_subscriptions.read().await;
        let source1_queries = subscriptions.get("source1").unwrap();

        assert_eq!(source1_queries.len(), 2);
        assert!(source1_queries.contains(&"query1".to_string()));
        assert!(source1_queries.contains(&"query2".to_string()));
    }

    #[tokio::test]
    async fn test_remove_subscription() {
        let router = DataRouter::new();

        let _rx = router
            .add_query_subscription("query1".to_string(), vec!["source1".to_string()])
            .await;

        // Verify subscription exists
        assert!(router.query_senders.read().await.contains_key("query1"));

        // Remove subscription
        router.remove_query_subscription("query1").await;

        // Verify removal
        let senders = router.query_senders.read().await;
        assert!(!senders.contains_key("query1"));
    }
}
