use super::super::*;
use crate::api::{Query, Reaction, Source};

// ========================================================================
// Tests for Component Creation Without Initialization
// ========================================================================

#[tokio::test]
async fn test_create_source_without_initialization() {
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
    let source = Source::application("runtime-source").build();

    let result = core.create_source(source).await;
    assert!(
        result.is_err(),
        "create_source should fail without initialization"
    );
    assert!(matches!(result.unwrap_err(), DrasiError::InvalidState(_)));
}

#[tokio::test]
async fn test_create_query_without_initialization() {
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
    let query = Query::cypher("runtime-query")
        .query("MATCH (n) RETURN n")
        .from_source("source1")
        .build();

    let result = core.create_query(query).await;
    assert!(
        result.is_err(),
        "create_query should fail without initialization"
    );
    assert!(matches!(result.unwrap_err(), DrasiError::InvalidState(_)));
}

#[tokio::test]
async fn test_create_reaction_without_initialization() {
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
    let reaction = Reaction::log("runtime-reaction")
        .subscribe_to("query1")
        .build();

    let result = core.create_reaction(reaction).await;
    assert!(
        result.is_err(),
        "create_reaction should fail without initialization"
    );
    assert!(matches!(result.unwrap_err(), DrasiError::InvalidState(_)));
}

// ========================================================================
// Tests for Component Removal Without Initialization
// ========================================================================

#[tokio::test]
async fn test_remove_source_without_initialization() {
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

    let result = core.remove_source("any-source").await;
    assert!(
        result.is_err(),
        "remove_source should fail without initialization"
    );
    assert!(matches!(result.unwrap_err(), DrasiError::InvalidState(_)));
}

#[tokio::test]
async fn test_remove_query_without_initialization() {
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

    let result = core.remove_query("any-query").await;
    assert!(
        result.is_err(),
        "remove_query should fail without initialization"
    );
    assert!(matches!(result.unwrap_err(), DrasiError::InvalidState(_)));
}

#[tokio::test]
async fn test_remove_reaction_without_initialization() {
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

    let result = core.remove_reaction("any-reaction").await;
    assert!(
        result.is_err(),
        "remove_reaction should fail without initialization"
    );
    assert!(matches!(result.unwrap_err(), DrasiError::InvalidState(_)));
}

// ========================================================================
// Tests for Removing Nonexistent Components
// ========================================================================

#[tokio::test]
async fn test_remove_nonexistent_source() {
    let mut core = DrasiServerCore::builder().build().await.unwrap();
    core.initialize().await.unwrap();

    let result = core.remove_source("nonexistent").await;
    assert!(result.is_err(), "Should fail to remove nonexistent source");
    assert!(matches!(
        result.unwrap_err(),
        DrasiError::ComponentNotFound { .. }
    ));
}

#[tokio::test]
async fn test_remove_nonexistent_query() {
    let mut core = DrasiServerCore::builder().build().await.unwrap();
    core.initialize().await.unwrap();

    let result = core.remove_query("nonexistent").await;
    assert!(result.is_err(), "Should fail to remove nonexistent query");
    assert!(matches!(
        result.unwrap_err(),
        DrasiError::ComponentNotFound { .. }
    ));
}

#[tokio::test]
async fn test_remove_nonexistent_reaction() {
    let mut core = DrasiServerCore::builder().build().await.unwrap();
    core.initialize().await.unwrap();

    let result = core.remove_reaction("nonexistent").await;
    assert!(
        result.is_err(),
        "Should fail to remove nonexistent reaction"
    );
    assert!(matches!(
        result.unwrap_err(),
        DrasiError::ComponentNotFound { .. }
    ));
}

// ========================================================================
// Tests for Component Listing After Operations
// ========================================================================

#[tokio::test]
async fn test_list_components_after_runtime_add() {
    let mut core = DrasiServerCore::builder().build().await.unwrap();
    core.initialize().await.unwrap();

    // Initially empty
    let sources = core.list_sources().await.unwrap();
    assert_eq!(sources.len(), 0);

    // Add a source at runtime
    core.create_source(Source::application("runtime-source").build())
        .await
        .unwrap();

    // Should now show in list
    let sources = core.list_sources().await.unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].0, "runtime-source");
}

#[tokio::test]
async fn test_list_components_after_removal() {
    let mut core = DrasiServerCore::builder()
        .add_source(Source::application("source1").build())
        .add_source(Source::application("source2").build())
        .build()
        .await
        .unwrap();

    core.initialize().await.unwrap();

    let sources = core.list_sources().await.unwrap();
    assert_eq!(sources.len(), 2);

    // Remove one source
    core.remove_source("source1").await.unwrap();

    // Should only show remaining source
    let sources = core.list_sources().await.unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].0, "source2");
}

// ========================================================================
// Tests for Component Start/Stop APIs
// ========================================================================

#[tokio::test]
async fn test_start_source() {
    let core = DrasiServerCore::builder()
        .add_source(Source::application("test-source").auto_start(false).build())
        .build()
        .await
        .unwrap();

    // Initially stopped
    let status = core.get_source_status("test-source").await.unwrap();
    assert!(matches!(status, ComponentStatus::Stopped));

    // Start it
    core.start_source("test-source").await.unwrap();

    // Should now be starting or running
    let status = core.get_source_status("test-source").await.unwrap();
    assert!(matches!(
        status,
        ComponentStatus::Starting | ComponentStatus::Running
    ));
}

#[tokio::test]
async fn test_stop_source() {
    let core = DrasiServerCore::builder()
        .add_source(Source::application("test-source").auto_start(true).build())
        .build()
        .await
        .unwrap();

    // Start the server to start auto-start components
    core.start().await.unwrap();

    // Should be running
    let status = core.get_source_status("test-source").await.unwrap();
    assert!(matches!(
        status,
        ComponentStatus::Running | ComponentStatus::Starting
    ));

    // Stop it
    core.stop_source("test-source").await.unwrap();

    // Should now be stopped
    let status = core.get_source_status("test-source").await.unwrap();
    assert!(matches!(
        status,
        ComponentStatus::Stopped | ComponentStatus::Stopping
    ));
}

#[tokio::test]
async fn test_start_source_not_found() {
    let core = DrasiServerCore::builder().build().await.unwrap();

    let result = core.start_source("nonexistent").await;
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        DrasiError::ComponentNotFound { .. }
    ));
}

#[tokio::test]
async fn test_start_query() {
    let core = DrasiServerCore::builder()
        .add_source(Source::application("source1").build())
        .add_query(
            Query::cypher("test-query")
                .query("MATCH (n) RETURN n")
                .from_source("source1")
                .auto_start(false)
                .build(),
        )
        .build()
        .await
        .unwrap();

    // Initially stopped
    let status = core.get_query_status("test-query").await.unwrap();
    assert!(matches!(status, ComponentStatus::Stopped));

    // Start it
    core.start_query("test-query").await.unwrap();

    // Should now be starting or running
    let status = core.get_query_status("test-query").await.unwrap();
    assert!(matches!(
        status,
        ComponentStatus::Starting | ComponentStatus::Running
    ));
}

#[tokio::test]
async fn test_stop_query() {
    let core = DrasiServerCore::builder()
        .add_source(Source::application("source1").build())
        .add_query(
            Query::cypher("test-query")
                .query("MATCH (n) RETURN n")
                .from_source("source1")
                .auto_start(true)
                .build(),
        )
        .build()
        .await
        .unwrap();

    // Start the server
    core.start().await.unwrap();

    // Should be running
    let status = core.get_query_status("test-query").await.unwrap();
    assert!(matches!(
        status,
        ComponentStatus::Running | ComponentStatus::Starting
    ));

    // Stop it
    core.stop_query("test-query").await.unwrap();

    // Should now be stopped
    let status = core.get_query_status("test-query").await.unwrap();
    assert!(matches!(
        status,
        ComponentStatus::Stopped | ComponentStatus::Stopping
    ));
}

#[tokio::test]
async fn test_start_query_not_found() {
    let core = DrasiServerCore::builder().build().await.unwrap();

    let result = core.start_query("nonexistent").await;
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        DrasiError::ComponentNotFound { .. }
    ));
}

#[tokio::test]
async fn test_start_reaction() {
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
                .auto_start(false)
                .build(),
        )
        .build()
        .await
        .unwrap();

    // Initially stopped
    let status = core.get_reaction_status("test-reaction").await.unwrap();
    assert!(matches!(status, ComponentStatus::Stopped));

    // Start it
    core.start_reaction("test-reaction").await.unwrap();

    // Should now be starting or running
    let status = core.get_reaction_status("test-reaction").await.unwrap();
    assert!(matches!(
        status,
        ComponentStatus::Starting | ComponentStatus::Running
    ));
}

#[tokio::test]
async fn test_stop_reaction() {
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
                .auto_start(true)
                .build(),
        )
        .build()
        .await
        .unwrap();

    // Start the server
    core.start().await.unwrap();

    // Should be running
    let status = core.get_reaction_status("test-reaction").await.unwrap();
    assert!(matches!(
        status,
        ComponentStatus::Running | ComponentStatus::Starting
    ));

    // Stop it
    core.stop_reaction("test-reaction").await.unwrap();

    // Should now be stopped
    let status = core.get_reaction_status("test-reaction").await.unwrap();
    assert!(matches!(
        status,
        ComponentStatus::Stopped | ComponentStatus::Stopping
    ));
}

#[tokio::test]
async fn test_start_reaction_not_found() {
    let core = DrasiServerCore::builder().build().await.unwrap();

    let result = core.start_reaction("nonexistent").await;
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        DrasiError::ComponentNotFound { .. }
    ));
}

#[tokio::test]
async fn test_start_source_without_initialization() {
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
    let result = core.start_source("any-source").await;

    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), DrasiError::InvalidState(_)));
}
