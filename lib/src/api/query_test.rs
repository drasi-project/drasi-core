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

//! Tests for QueryBuilder

#[cfg(test)]
mod tests {
    use crate::api::Query;
    use crate::config::{QueryJoinConfig, QueryJoinKeyConfig, QueryLanguage};

    #[test]
    fn test_query_cypher() {
        let query = Query::cypher("cypher-query").build();

        assert_eq!(query.id, "cypher-query");
        assert!(matches!(query.query_language, QueryLanguage::Cypher));
        assert_eq!(query.query, "");
        assert!(query.source_subscriptions.is_empty());
        assert!(query.auto_start);
        assert!(query.joins.is_none());
    }

    #[test]
    fn test_query_gql() {
        let query = Query::gql("gql-query").build();

        assert_eq!(query.id, "gql-query");
        assert!(matches!(query.query_language, QueryLanguage::GQL));
        assert_eq!(query.query, "");
        assert!(query.source_subscriptions.is_empty());
        assert!(query.auto_start);
    }

    #[test]
    fn test_query_with_query_string() {
        let query = Query::cypher("test-query")
            .query("MATCH (n:User) WHERE n.active = true RETURN n")
            .build();

        assert_eq!(query.query, "MATCH (n:User) WHERE n.active = true RETURN n");
    }

    #[test]
    fn test_query_from_single_source() {
        let query = Query::cypher("test-query")
            .query("MATCH (n) RETURN n")
            .from_source("source-1")
            .build();

        assert_eq!(query.source_subscriptions.len(), 1);
        assert_eq!(query.source_subscriptions[0].source_id, "source-1");
    }

    #[test]
    fn test_query_from_multiple_sources_individually() {
        let query = Query::cypher("test-query")
            .query("MATCH (n) RETURN n")
            .from_source("source-1")
            .from_source("source-2")
            .from_source("source-3")
            .build();

        assert_eq!(query.source_subscriptions.len(), 3);
        assert_eq!(query.source_subscriptions[0].source_id, "source-1");
        assert_eq!(query.source_subscriptions[1].source_id, "source-2");
        assert_eq!(query.source_subscriptions[2].source_id, "source-3");
    }

    #[test]
    fn test_query_from_sources_vec() {
        let sources = vec!["source-1".to_string(), "source-2".to_string()];
        let query = Query::cypher("test-query")
            .query("MATCH (n) RETURN n")
            .from_sources(sources)
            .build();

        assert_eq!(query.source_subscriptions.len(), 2);
        assert_eq!(query.source_subscriptions[0].source_id, "source-1");
        assert_eq!(query.source_subscriptions[1].source_id, "source-2");
    }

    #[test]
    fn test_query_auto_start_false() {
        let query = Query::cypher("test-query")
            .query("MATCH (n) RETURN n")
            .auto_start(false)
            .build();

        assert!(!query.auto_start);
    }

    #[test]
    fn test_query_auto_start_true() {
        let query = Query::cypher("test-query")
            .query("MATCH (n) RETURN n")
            .auto_start(true)
            .build();

        assert!(query.auto_start);
    }

    #[test]
    fn test_query_with_single_join() {
        let join = QueryJoinConfig {
            id: "join-1".to_string(),
            keys: vec![],
        };

        let query = Query::cypher("test-query")
            .query("MATCH (n) RETURN n")
            .with_join(join)
            .build();

        assert!(query.joins.is_some());
        let joins = query.joins.unwrap();
        assert_eq!(joins.len(), 1);
        assert_eq!(joins[0].id, "join-1");
    }

    #[test]
    fn test_query_with_multiple_joins_individually() {
        let join1 = QueryJoinConfig {
            id: "join-1".to_string(),
            keys: vec![],
        };

        let join2 = QueryJoinConfig {
            id: "join-2".to_string(),
            keys: vec![],
        };

        let query = Query::cypher("test-query")
            .query("MATCH (n) RETURN n")
            .with_join(join1)
            .with_join(join2)
            .build();

        assert!(query.joins.is_some());
        let joins = query.joins.unwrap();
        assert_eq!(joins.len(), 2);
        assert_eq!(joins[0].id, "join-1");
        assert_eq!(joins[1].id, "join-2");
    }

    #[test]
    fn test_query_with_joins_vec() {
        let joins_vec = vec![
            QueryJoinConfig {
                id: "join-1".to_string(),
                keys: vec![],
            },
            QueryJoinConfig {
                id: "join-2".to_string(),
                keys: vec![],
            },
        ];

        let query = Query::cypher("test-query")
            .query("MATCH (n) RETURN n")
            .with_joins(joins_vec)
            .build();

        assert!(query.joins.is_some());
        let joins = query.joins.unwrap();
        assert_eq!(joins.len(), 2);
    }

    #[test]
    fn test_query_builder_chaining() {
        let query = Query::cypher("chained-query")
            .query("MATCH (n:Order) WHERE n.status = 'active' RETURN n")
            .from_source("orders-source")
            .auto_start(true)
            .build();

        assert_eq!(query.id, "chained-query");
        assert_eq!(
            query.query,
            "MATCH (n:Order) WHERE n.status = 'active' RETURN n"
        );
        assert_eq!(query.source_subscriptions.len(), 1);
        assert!(query.auto_start);
    }

    #[test]
    fn test_query_complex_config() {
        let joins_vec = vec![QueryJoinConfig {
            id: "join-1".to_string(),
            keys: vec![
                QueryJoinKeyConfig {
                    label: "User".to_string(),
                    property: "id".to_string(),
                },
                QueryJoinKeyConfig {
                    label: "Order".to_string(),
                    property: "user_id".to_string(),
                },
            ],
        }];

        let query = Query::cypher("complex-query")
            .query("MATCH (n)-[r]->(m) RETURN n, r, m")
            .from_sources(vec!["source-1".to_string(), "source-2".to_string()])
            .auto_start(true)
            .with_joins(joins_vec)
            .build();

        assert_eq!(query.id, "complex-query");
        assert_eq!(query.source_subscriptions.len(), 2);
        assert!(query.joins.is_some());
        assert_eq!(query.joins.unwrap().len(), 1);
    }

    #[test]
    fn test_query_gql_complete() {
        let query = Query::gql("gql-query")
            .query("{ users { id name email } }")
            .from_source("users-source")
            .auto_start(true)
            .build();

        assert_eq!(query.id, "gql-query");
        assert!(matches!(query.query_language, QueryLanguage::GQL));
        assert_eq!(query.query, "{ users { id name email } }");
        assert_eq!(query.source_subscriptions.len(), 1);
    }

    #[test]
    fn test_query_sources_combined() {
        // Combining from_source and from_sources should accumulate sources
        let query = Query::cypher("test-query")
            .query("MATCH (n) RETURN n")
            .from_source("source-1")
            .from_sources(vec!["source-2".to_string(), "source-3".to_string()])
            .build();

        assert_eq!(query.source_subscriptions.len(), 3);
        assert_eq!(query.source_subscriptions[0].source_id, "source-1");
        assert_eq!(query.source_subscriptions[1].source_id, "source-2");
        assert_eq!(query.source_subscriptions[2].source_id, "source-3");
    }

    #[test]
    fn test_query_with_joins_replaces_previous() {
        // with_joins should replace previous joins
        let join1 = QueryJoinConfig {
            id: "join-1".to_string(),
            keys: vec![],
        };

        let join2 = QueryJoinConfig {
            id: "join-2".to_string(),
            keys: vec![],
        };

        let query = Query::cypher("test-query")
            .query("MATCH (n) RETURN n")
            .with_join(join1)
            .with_joins(vec![join2])
            .build();

        let joins = query.joins.unwrap();
        assert_eq!(joins.len(), 1);
        assert_eq!(joins[0].id, "join-2");
    }

    #[test]
    fn test_query_overwrite_query_string() {
        // Later query() call should replace earlier one
        let query = Query::cypher("test-query")
            .query("MATCH (n:Old) RETURN n")
            .query("MATCH (n:New) RETURN n")
            .build();

        assert_eq!(query.query, "MATCH (n:New) RETURN n");
    }
}
