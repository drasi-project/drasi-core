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

//! Tests for DrasiServerCoreBuilder

#[cfg(test)]
mod tests {
    use crate::api::{builder::DrasiServerCoreBuilder, Query, Reaction, Source};

    #[tokio::test]
    async fn test_builder_creates_default_instance() {
        let builder = DrasiServerCoreBuilder::new();
        let core = builder.build().await;

        assert!(
            core.is_ok(),
            "Builder should create server with no components"
        );
    }

    #[tokio::test]
    async fn test_builder_with_id() {
        let builder = DrasiServerCoreBuilder::new().with_id("test-server-123");
        let core = builder.build().await;

        assert!(core.is_ok(), "Builder should create server with custom ID");
    }

    #[tokio::test]
    async fn test_builder_add_single_source() {
        let source = Source::application("test-source").build();

        let builder = DrasiServerCoreBuilder::new().add_source(source);
        let core = builder.build().await;

        assert!(
            core.is_ok(),
            "Builder should create server with single source"
        );
    }

    #[tokio::test]
    async fn test_builder_add_multiple_sources() {
        let sources = vec![
            Source::application("source-1").build(),
            Source::application("source-2").build(),
            Source::mock("source-3").build(),
        ];

        let builder = DrasiServerCoreBuilder::new().add_sources(sources);
        let core = builder.build().await;

        assert!(
            core.is_ok(),
            "Builder should create server with multiple sources"
        );
    }

    #[tokio::test]
    async fn test_builder_add_single_query() {
        let source = Source::application("test-source").build();
        let query = Query::cypher("test-query")
            .query("MATCH (n) RETURN n")
            .from_source("test-source")
            .build();

        let builder = DrasiServerCoreBuilder::new()
            .add_source(source)
            .add_query(query);
        let core = builder.build().await;

        assert!(core.is_ok(), "Builder should create server with query");
    }

    #[tokio::test]
    async fn test_builder_add_multiple_queries() {
        let source = Source::application("test-source").build();
        let queries = vec![
            Query::cypher("query-1")
                .query("MATCH (n:Node) RETURN n")
                .from_source("test-source")
                .build(),
            Query::cypher("query-2")
                .query("MATCH (n:User) RETURN n")
                .from_source("test-source")
                .build(),
        ];

        let builder = DrasiServerCoreBuilder::new()
            .add_source(source)
            .add_queries(queries);
        let core = builder.build().await;

        assert!(
            core.is_ok(),
            "Builder should create server with multiple queries"
        );
    }

    #[tokio::test]
    async fn test_builder_add_single_reaction() {
        let source = Source::application("test-source").build();
        let query = Query::cypher("test-query")
            .query("MATCH (n) RETURN n")
            .from_source("test-source")
            .build();
        let reaction = Reaction::log("test-reaction")
            .subscribe_to("test-query")
            .build();

        let builder = DrasiServerCoreBuilder::new()
            .add_source(source)
            .add_query(query)
            .add_reaction(reaction);
        let core = builder.build().await;

        assert!(core.is_ok(), "Builder should create server with reaction");
    }

    #[tokio::test]
    async fn test_builder_add_multiple_reactions() {
        let source = Source::application("test-source").build();
        let query = Query::cypher("test-query")
            .query("MATCH (n) RETURN n")
            .from_source("test-source")
            .build();
        let reactions = vec![
            Reaction::log("reaction-1")
                .subscribe_to("test-query")
                .build(),
            Reaction::application("reaction-2")
                .subscribe_to("test-query")
                .build(),
        ];

        let builder = DrasiServerCoreBuilder::new()
            .add_source(source)
            .add_query(query)
            .add_reactions(reactions);
        let core = builder.build().await;

        assert!(
            core.is_ok(),
            "Builder should create server with multiple reactions"
        );
    }

    #[tokio::test]
    async fn test_builder_complete_pipeline() {
        let builder = DrasiServerCoreBuilder::new()
            .with_id("full-pipeline")
            .add_source(Source::application("app-source").build())
            .add_query(
                Query::cypher("orders-query")
                    .query("MATCH (o:Order) WHERE o.status = 'active' RETURN o")
                    .from_source("app-source")
                    .build(),
            )
            .add_reaction(
                Reaction::log("log-reaction")
                    .subscribe_to("orders-query")
                    .build(),
            );

        let core = builder.build().await;
        assert!(core.is_ok(), "Builder should create complete pipeline");
    }

    #[tokio::test]
    async fn test_builder_chaining() {
        // Test builder method chaining
        let core = DrasiServerCoreBuilder::new()
            .with_id("chained-builder")
            .add_source(Source::application("source-1").build())
            .add_source(Source::mock("source-2").build())
            .add_query(
                Query::cypher("query-1")
                    .query("MATCH (n) RETURN n")
                    .from_source("source-1")
                    .build(),
            )
            .add_reaction(Reaction::log("reaction-1").subscribe_to("query-1").build())
            .build()
            .await;

        assert!(core.is_ok(), "Chained builder methods should work");
    }

    #[tokio::test]
    async fn test_builder_multi_source_pipeline() {
        let core = DrasiServerCoreBuilder::new()
            .add_source(Source::application("source-1").build())
            .add_source(Source::application("source-2").build())
            .add_query(
                Query::cypher("multi-query")
                    .query("MATCH (n) RETURN n")
                    .from_sources(vec!["source-1".to_string(), "source-2".to_string()])
                    .build(),
            )
            .add_reaction(
                Reaction::application("app-reaction")
                    .subscribe_to("multi-query")
                    .build(),
            )
            .build()
            .await;

        assert!(
            core.is_ok(),
            "Multi-source pipeline should build successfully"
        );
    }
}
