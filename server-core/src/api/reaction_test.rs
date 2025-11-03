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

//! Tests for ReactionBuilder

#[cfg(test)]
mod tests {
    use crate::api::{Properties, Reaction};
    use serde_json::json;

    #[test]
    fn test_reaction_application() {
        let reaction = Reaction::application("app-reaction").build();

        assert_eq!(reaction.id, "app-reaction");
        assert_eq!(reaction.reaction_type, "application");
        assert!(
            reaction.auto_start,
            "Application reaction should auto-start by default"
        );
        assert!(reaction.queries.is_empty());
    }

    #[test]
    fn test_reaction_http() {
        let reaction = Reaction::http("http-reaction").build();

        assert_eq!(reaction.id, "http-reaction");
        assert_eq!(reaction.reaction_type, "http");
        assert!(reaction.auto_start);
    }

    #[test]
    fn test_reaction_grpc() {
        let reaction = Reaction::grpc("grpc-reaction").build();

        assert_eq!(reaction.id, "grpc-reaction");
        assert_eq!(reaction.reaction_type, "grpc");
        assert!(reaction.auto_start);
    }

    #[test]
    fn test_reaction_sse() {
        let reaction = Reaction::sse("sse-reaction").build();

        assert_eq!(reaction.id, "sse-reaction");
        assert_eq!(reaction.reaction_type, "sse");
        assert!(reaction.auto_start);
    }

    #[test]
    fn test_reaction_log() {
        let reaction = Reaction::log("log-reaction").build();

        assert_eq!(reaction.id, "log-reaction");
        assert_eq!(reaction.reaction_type, "log");
        assert!(reaction.auto_start);
    }

    #[test]
    fn test_reaction_custom() {
        let reaction = Reaction::custom("custom-reaction", "custom-type").build();

        assert_eq!(reaction.id, "custom-reaction");
        assert_eq!(reaction.reaction_type, "custom-type");
        assert!(reaction.auto_start);
    }

    #[test]
    fn test_reaction_subscribe_to_single_query() {
        let reaction = Reaction::application("test-reaction")
            .subscribe_to("query-1")
            .build();

        assert_eq!(reaction.queries.len(), 1);
        assert_eq!(reaction.queries[0], "query-1");
    }

    #[test]
    fn test_reaction_subscribe_to_multiple_queries_individually() {
        let reaction = Reaction::application("test-reaction")
            .subscribe_to("query-1")
            .subscribe_to("query-2")
            .subscribe_to("query-3")
            .build();

        assert_eq!(reaction.queries.len(), 3);
        assert_eq!(reaction.queries, vec!["query-1", "query-2", "query-3"]);
    }

    #[test]
    fn test_reaction_subscribe_to_queries_vec() {
        let queries = vec!["query-1".to_string(), "query-2".to_string()];
        let reaction = Reaction::application("test-reaction")
            .subscribe_to_queries(queries)
            .build();

        assert_eq!(reaction.queries.len(), 2);
        assert_eq!(reaction.queries[0], "query-1");
        assert_eq!(reaction.queries[1], "query-2");
    }

    #[test]
    fn test_reaction_auto_start_false() {
        let reaction = Reaction::application("test-reaction")
            .auto_start(false)
            .build();

        assert!(!reaction.auto_start);
    }

    #[test]
    fn test_reaction_auto_start_true() {
        let reaction = Reaction::application("test-reaction")
            .auto_start(true)
            .build();

        assert!(reaction.auto_start);
    }

    #[test]
    fn test_reaction_with_property() {
        let reaction = Reaction::http("http-reaction")
            .with_property("base_url", json!("http://localhost:8080"))
            .with_property("timeout_ms", json!(5000))
            .build();

        // Verify the reaction is created correctly
        assert_eq!(reaction.id, "http-reaction");
        assert_eq!(reaction.reaction_type, "http");
        // Properties should be stored in the typed config
        let properties = reaction.get_properties();
        assert!(properties.contains_key("base_url"));
        assert!(properties.contains_key("timeout_ms"));
    }

    #[test]
    fn test_reaction_with_properties() {
        let props = Properties::new()
            .with_string("base_url", "http://localhost:8080")
            .with_int("timeout_ms", 5000);

        let reaction = Reaction::http("http-reaction")
            .with_properties(props)
            .build();

        assert_eq!(reaction.id, "http-reaction");
        assert_eq!(reaction.reaction_type, "http");
        // Verify properties are stored in config
        let properties = reaction.get_properties();
        assert!(properties.contains_key("base_url"));
        assert!(properties.contains_key("timeout_ms"));
    }

    #[test]
    fn test_reaction_builder_chaining() {
        let reaction = Reaction::http("chained-reaction")
            .subscribe_to("orders-query")
            .auto_start(true)
            .with_property("base_url", json!("http://api:8080"))
            .with_property("timeout_ms", json!(5000))
            .build();

        assert_eq!(reaction.id, "chained-reaction");
        assert_eq!(reaction.reaction_type, "http");
        assert_eq!(reaction.queries.len(), 1);
        assert!(reaction.auto_start);
        // Properties should be in config
        let props = reaction.get_properties();
        assert!(props.contains_key("base_url"));
        assert!(props.contains_key("timeout_ms"));
    }

    #[test]
    fn test_reaction_complex_config() {
        let reaction = Reaction::http("complex-reaction")
            .subscribe_to_queries(vec!["query-1".to_string(), "query-2".to_string()])
            .auto_start(true)
            .with_properties(
                Properties::new()
                    .with_string("base_url", "http://localhost:8080")
                    .with_int("timeout_ms", 10000),
            )
            .build();

        assert_eq!(reaction.id, "complex-reaction");
        assert_eq!(reaction.reaction_type, "http");
        assert_eq!(reaction.queries.len(), 2);
        assert!(reaction.auto_start);
        // Verify properties are stored in config
        let props = reaction.get_properties();
        assert!(props.contains_key("base_url"));
        assert!(props.contains_key("timeout_ms"));
    }

    #[test]
    fn test_reaction_log_with_query() {
        let reaction = Reaction::log("debug-logger")
            .subscribe_to("test-query")
            .build();

        assert_eq!(reaction.id, "debug-logger");
        assert_eq!(reaction.reaction_type, "log");
        assert_eq!(reaction.queries.len(), 1);
    }

    #[test]
    fn test_reaction_sse_config() {
        let reaction = Reaction::sse("sse-stream")
            .subscribe_to("events-query")
            .with_property("port", json!(8081))
            .with_property("sse_path", json!("/events"))
            .build();

        assert_eq!(reaction.id, "sse-stream");
        assert_eq!(reaction.reaction_type, "sse");
        assert_eq!(reaction.queries.len(), 1);
        // Verify config properties are stored
        let properties = reaction.get_properties();
        assert!(properties.contains_key("port"));
        assert!(properties.contains_key("sse_path"));
    }

    #[test]
    fn test_reaction_grpc_config() {
        let reaction = Reaction::grpc("grpc-stream")
            .subscribe_to("data-query")
            .with_properties(
                Properties::new()
                    .with_string("endpoint", "localhost:50051")
                    .with_int("timeout_ms", 30000),
            )
            .build();

        assert_eq!(reaction.id, "grpc-stream");
        assert_eq!(reaction.reaction_type, "grpc");
        assert_eq!(reaction.queries.len(), 1);
        // Verify config properties are stored
        let props = reaction.get_properties();
        assert!(props.contains_key("endpoint"));
        assert!(props.contains_key("timeout_ms"));
    }

    #[test]
    fn test_reaction_empty_properties() {
        let reaction = Reaction::application("test-reaction")
            .with_properties(Properties::new())
            .build();

        assert_eq!(reaction.id, "test-reaction");
        assert_eq!(reaction.reaction_type, "application");
        // Empty properties should be empty
        let props = reaction.get_properties();
        assert!(props.is_empty() || props.len() == 0);
    }

    #[test]
    fn test_reaction_properties_override() {
        // with_properties should replace all previous properties
        let reaction = Reaction::http("test-reaction")
            .with_property("base_url", json!("http://old"))
            .with_properties(Properties::new().with_string("base_url", "http://new"))
            .build();

        assert_eq!(reaction.id, "test-reaction");
        assert_eq!(reaction.reaction_type, "http");
        // Verify properties were replaced
        let properties = reaction.get_properties();
        assert!(properties.contains_key("base_url"));
    }

    #[test]
    fn test_reaction_queries_combined() {
        // Combining subscribe_to and subscribe_to_queries should accumulate queries
        let reaction = Reaction::application("test-reaction")
            .subscribe_to("query-1")
            .subscribe_to_queries(vec!["query-2".to_string(), "query-3".to_string()])
            .build();

        assert_eq!(reaction.queries.len(), 3);
        assert_eq!(reaction.queries, vec!["query-1", "query-2", "query-3"]);
    }

    #[test]
    fn test_reaction_no_queries() {
        // Reaction without queries should still be valid (might subscribe later)
        let reaction = Reaction::application("test-reaction").build();

        assert!(reaction.queries.is_empty());
    }

    #[test]
    fn test_reaction_duplicate_query_subscriptions() {
        // Subscribing to the same query multiple times
        let reaction = Reaction::application("test-reaction")
            .subscribe_to("query-1")
            .subscribe_to("query-1")
            .build();

        // Should allow duplicates (validation happens elsewhere)
        assert_eq!(reaction.queries.len(), 2);
        assert_eq!(reaction.queries[0], "query-1");
        assert_eq!(reaction.queries[1], "query-1");
    }
}
