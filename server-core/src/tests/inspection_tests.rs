use super::super::*;
use crate::api::{Query, Reaction, Source};

#[tokio::test]
async fn test_list_sources() {
    let core = DrasiServerCore::builder()
        .add_source(Source::application("source1").build())
        .add_source(Source::mock("source2").build())
        .build()
        .await
        .unwrap();

    let sources = core.list_sources().await.unwrap();
    assert_eq!(sources.len(), 2);

    let source_ids: Vec<String> = sources.iter().map(|(id, _)| id.clone()).collect();
    assert!(source_ids.contains(&"source1".to_string()));
    assert!(source_ids.contains(&"source2".to_string()));
}

#[tokio::test]
async fn test_list_sources_without_initialization() {
    let config = Arc::new(RuntimeConfig {
        server_core: crate::config::DrasiServerCoreSettings {
            id: "test-server".to_string(),
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
        },
        sources: vec![],
        queries: vec![],
        reactions: vec![],
    });

    let core = DrasiServerCore::new(config);
    let result = core.list_sources().await;

    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), DrasiError::InvalidState(_)));
}

#[tokio::test]
async fn test_get_source_info() {
    let core = DrasiServerCore::builder()
        .add_source(Source::application("test-source").build())
        .build()
        .await
        .unwrap();

    let source_info = core.get_source_info("test-source").await.unwrap();
    assert_eq!(source_info.id, "test-source");
    assert_eq!(source_info.source_type, "application");
}

#[tokio::test]
async fn test_get_source_info_not_found() {
    let core = DrasiServerCore::builder().build().await.unwrap();

    let result = core.get_source_info("nonexistent").await;
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        DrasiError::ComponentNotFound { .. }
    ));
}

#[tokio::test]
async fn test_get_source_status() {
    let core = DrasiServerCore::builder()
        .add_source(Source::application("test-source").build())
        .build()
        .await
        .unwrap();

    let status = core.get_source_status("test-source").await.unwrap();
    assert!(matches!(status, ComponentStatus::Stopped));
}

#[tokio::test]
async fn test_list_queries() {
    let core = DrasiServerCore::builder()
        .add_source(Source::application("source1").build())
        .add_query(
            Query::cypher("query1")
                .query("MATCH (n) RETURN n")
                .from_source("source1")
                .build(),
        )
        .add_query(
            Query::cypher("query2")
                .query("MATCH (n) RETURN n.name")
                .from_source("source1")
                .build(),
        )
        .build()
        .await
        .unwrap();

    let queries = core.list_queries().await.unwrap();
    assert_eq!(queries.len(), 2);

    let query_ids: Vec<String> = queries.iter().map(|(id, _)| id.clone()).collect();
    assert!(query_ids.contains(&"query1".to_string()));
    assert!(query_ids.contains(&"query2".to_string()));
}

#[tokio::test]
async fn test_get_query_info() {
    let core = DrasiServerCore::builder()
        .add_source(Source::application("source1").build())
        .add_query(
            Query::cypher("test-query")
                .query("MATCH (n) RETURN n")
                .from_source("source1")
                .build(),
        )
        .build()
        .await
        .unwrap();

    let query_info = core.get_query_info("test-query").await.unwrap();
    assert_eq!(query_info.id, "test-query");
    assert_eq!(query_info.query, "MATCH (n) RETURN n");
    assert!(query_info.source_subscriptions.iter().any(|s| s.source_id == "source1"));
}

#[tokio::test]
async fn test_get_query_info_not_found() {
    let core = DrasiServerCore::builder().build().await.unwrap();

    let result = core.get_query_info("nonexistent").await;
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        DrasiError::ComponentNotFound { .. }
    ));
}

#[tokio::test]
async fn test_get_query_status() {
    let core = DrasiServerCore::builder()
        .add_source(Source::application("source1").build())
        .add_query(
            Query::cypher("test-query")
                .query("MATCH (n) RETURN n")
                .from_source("source1")
                .build(),
        )
        .build()
        .await
        .unwrap();

    let status = core.get_query_status("test-query").await.unwrap();
    assert!(matches!(status, ComponentStatus::Stopped));
}

#[tokio::test]
async fn test_get_query_results_not_running() {
    let core = DrasiServerCore::builder()
        .add_source(Source::application("source1").build())
        .add_query(
            Query::cypher("test-query")
                .query("MATCH (n) RETURN n")
                .from_source("source1")
                .build(),
        )
        .build()
        .await
        .unwrap();

    let result = core.get_query_results("test-query").await;
    assert!(result.is_err());
    // Query must be running to get results
    let err = result.unwrap_err();
    assert!(matches!(err, DrasiError::InvalidState(_)));
}

#[tokio::test]
async fn test_list_reactions() {
    let core = DrasiServerCore::builder()
        .add_source(Source::application("source1").build())
        .add_query(
            Query::cypher("query1")
                .query("MATCH (n) RETURN n")
                .from_source("source1")
                .build(),
        )
        .add_reaction(Reaction::log("reaction1").subscribe_to("query1").build())
        .add_reaction(
            Reaction::application("reaction2")
                .subscribe_to("query1")
                .build(),
        )
        .build()
        .await
        .unwrap();

    let reactions = core.list_reactions().await.unwrap();
    assert_eq!(reactions.len(), 2);

    let reaction_ids: Vec<String> = reactions.iter().map(|(id, _)| id.clone()).collect();
    assert!(reaction_ids.contains(&"reaction1".to_string()));
    assert!(reaction_ids.contains(&"reaction2".to_string()));
}

#[tokio::test]
async fn test_get_reaction_info() {
    let core = DrasiServerCore::builder()
        .add_source(Source::application("source1").build())
        .add_query(
            Query::cypher("query1")
                .query("MATCH (n) RETURN n")
                .from_source("source1")
                .build(),
        )
        .add_reaction(
            Reaction::log("test-reaction")
                .subscribe_to("query1")
                .build(),
        )
        .build()
        .await
        .unwrap();

    let reaction_info = core.get_reaction_info("test-reaction").await.unwrap();
    assert_eq!(reaction_info.id, "test-reaction");
    assert_eq!(reaction_info.reaction_type, "log");
    assert!(reaction_info.queries.contains(&"query1".to_string()));
}

#[tokio::test]
async fn test_get_reaction_info_not_found() {
    let core = DrasiServerCore::builder().build().await.unwrap();

    let result = core.get_reaction_info("nonexistent").await;
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        DrasiError::ComponentNotFound { .. }
    ));
}

#[tokio::test]
async fn test_get_reaction_status() {
    let core = DrasiServerCore::builder()
        .add_source(Source::application("source1").build())
        .add_query(
            Query::cypher("query1")
                .query("MATCH (n) RETURN n")
                .from_source("source1")
                .build(),
        )
        .add_reaction(
            Reaction::log("test-reaction")
                .subscribe_to("query1")
                .build(),
        )
        .build()
        .await
        .unwrap();

    let status = core.get_reaction_status("test-reaction").await.unwrap();
    assert!(matches!(status, ComponentStatus::Stopped));
}

#[tokio::test]
async fn test_listing_empty_components() {
    let core = DrasiServerCore::builder().build().await.unwrap();

    let sources = core.list_sources().await.unwrap();
    assert_eq!(sources.len(), 0);

    let queries = core.list_queries().await.unwrap();
    assert_eq!(queries.len(), 0);

    let reactions = core.list_reactions().await.unwrap();
    assert_eq!(reactions.len(), 0);
}
