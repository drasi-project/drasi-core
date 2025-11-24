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

use super::*;
use std::collections::HashMap;

fn create_test_reaction_config(id: &str, queries: Vec<String>) -> ReactionConfig {
    use crate::reactions::application::ApplicationReactionConfig;

    ReactionConfig {
        id: id.to_string(),
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
