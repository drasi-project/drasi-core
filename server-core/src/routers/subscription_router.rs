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

use log::{debug, error, info};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

use crate::channels::{QueryResultReceiver, QueryResultSender};

/// Routes query results to subscribed reactions
pub struct SubscriptionRouter {
    /// Map of reaction id to its sender channel
    reaction_senders: Arc<RwLock<HashMap<String, QueryResultSender>>>,
    /// Map of query id to list of reaction ids that subscribe to it
    query_subscriptions: Arc<RwLock<HashMap<String, Vec<String>>>>,
}

impl Default for SubscriptionRouter {
    fn default() -> Self {
        Self {
            reaction_senders: Arc::new(RwLock::new(HashMap::new())),
            query_subscriptions: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl SubscriptionRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a reaction subscription to specific queries
    pub async fn add_reaction_subscription(
        &self,
        reaction_id: String,
        query_ids: Vec<String>,
    ) -> QueryResultReceiver {
        let start_time = std::time::Instant::now();
        info!(
            "Starting to add subscription for reaction '{}' to queries: {:?}",
            reaction_id, query_ids
        );

        let (tx, rx) = mpsc::channel(1000);

        // Store the sender for this reaction
        self.reaction_senders
            .write()
            .await
            .insert(reaction_id.clone(), tx);
        info!(
            "Channel created and sender stored for reaction '{}'",
            reaction_id
        );

        // Update query subscriptions
        let mut subscriptions = self.query_subscriptions.write().await;
        for query_id in &query_ids {
            subscriptions
                .entry(query_id.clone())
                .or_insert_with(Vec::new)
                .push(reaction_id.clone());

            info!(
                "Reaction '{}' successfully subscribed to query '{}'",
                reaction_id, query_id
            );
        }

        let elapsed = start_time.elapsed();
        info!(
            "Completed subscription for reaction '{}' to {} queries in {:?}",
            reaction_id,
            query_ids.len(),
            elapsed
        );

        rx
    }

    /// Remove a reaction subscription
    pub async fn remove_reaction_subscription(&self, reaction_id: &str) {
        // Remove the sender
        self.reaction_senders.write().await.remove(reaction_id);

        // Remove from all query subscriptions
        let mut subscriptions = self.query_subscriptions.write().await;
        for reactions in subscriptions.values_mut() {
            reactions.retain(|id| id != reaction_id);
        }

        info!("Removed subscription for reaction '{}'", reaction_id);
    }

    /// Start the router that receives query results and forwards them to subscribed reactions
    pub async fn start(&self, mut query_result_rx: QueryResultReceiver) {
        info!("Starting subscription router");

        while let Some(query_result) = query_result_rx.recv().await {
            let query_id = &query_result.query_id;
            debug!(
                "Router received result from query '{}' with {} items",
                query_id,
                query_result.results.len()
            );

            // Get list of reactions subscribed to this query
            let subscriptions = self.query_subscriptions.read().await;
            if let Some(reaction_ids) = subscriptions.get(query_id) {
                let senders = self.reaction_senders.read().await;

                for reaction_id in reaction_ids {
                    if let Some(sender) = senders.get(reaction_id) {
                        // Clone the query result for each reaction
                        match sender.send(query_result.clone()).await {
                            Ok(_) => {
                                debug!(
                                    "Routed query '{}' result to reaction '{}'",
                                    query_id, reaction_id
                                );
                            }
                            Err(e) => {
                                error!(
                                    "Failed to send result to reaction '{}': {}",
                                    reaction_id, e
                                );
                            }
                        }
                    }
                }
            } else {
                debug!("No reactions subscribed to query '{}'", query_id);
            }
        }

        info!("Subscription router stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_new_subscription_router() {
        let router = SubscriptionRouter::new();

        // Verify initial state
        let senders = router.reaction_senders.read().await;
        let subscriptions = router.query_subscriptions.read().await;

        assert!(senders.is_empty());
        assert!(subscriptions.is_empty());
    }

    #[tokio::test]
    async fn test_add_reaction_subscription() {
        let router = SubscriptionRouter::new();

        let rx = router
            .add_reaction_subscription("reaction1".to_string(), vec!["query1".to_string()])
            .await;

        // Verify reaction sender is stored
        let senders = router.reaction_senders.read().await;
        assert!(senders.contains_key("reaction1"));

        // Verify query subscription is recorded
        let subscriptions = router.query_subscriptions.read().await;
        assert_eq!(
            subscriptions.get("query1"),
            Some(&vec!["reaction1".to_string()])
        );

        // Verify receiver is functional
        assert!(!rx.is_closed());
    }

    #[tokio::test]
    async fn test_multiple_reactions_same_query() {
        let router = SubscriptionRouter::new();

        let _rx1 = router
            .add_reaction_subscription("reaction1".to_string(), vec!["query1".to_string()])
            .await;

        let _rx2 = router
            .add_reaction_subscription("reaction2".to_string(), vec!["query1".to_string()])
            .await;

        let subscriptions = router.query_subscriptions.read().await;
        let query1_reactions = subscriptions.get("query1").unwrap();

        assert_eq!(query1_reactions.len(), 2);
        assert!(query1_reactions.contains(&"reaction1".to_string()));
        assert!(query1_reactions.contains(&"reaction2".to_string()));
    }

    #[tokio::test]
    async fn test_remove_reaction_subscription() {
        let router = SubscriptionRouter::new();

        let _rx = router
            .add_reaction_subscription("reaction1".to_string(), vec!["query1".to_string()])
            .await;

        // Verify subscription exists
        assert!(router
            .reaction_senders
            .read()
            .await
            .contains_key("reaction1"));

        // Remove subscription
        router.remove_reaction_subscription("reaction1").await;

        // Verify removal
        let senders = router.reaction_senders.read().await;
        assert!(!senders.contains_key("reaction1"));
    }
}
