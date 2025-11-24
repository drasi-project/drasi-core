use super::super::*;
use crate::api::{Query, Reaction, Source};

#[tokio::test]
async fn test_server_initialization() {
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

    let mut core = DrasiServerCore::new(config);
    let result = core.initialize().await;

    assert!(result.is_ok(), "Server initialization should succeed");
    assert!(
        *core.initialized.read().await,
        "Server should be marked as initialized"
    );
}

#[tokio::test]
async fn test_server_initialization_idempotent() {
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

    let mut core = DrasiServerCore::new(config);

    // First initialization
    let result1 = core.initialize().await;
    assert!(result1.is_ok(), "First initialization should succeed");

    // Second initialization should also succeed (idempotent)
    let result2 = core.initialize().await;
    assert!(
        result2.is_ok(),
        "Second initialization should succeed (idempotent)"
    );
}

#[tokio::test]
async fn test_start_without_initialization_fails() {
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
    let result = core.start().await;

    assert!(result.is_err(), "Start should fail without initialization");
    assert!(result.unwrap_err().to_string().contains("initialized"));
}

#[tokio::test]
async fn test_start_already_running_fails() {
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

    let mut core = DrasiServerCore::new(config);
    core.initialize().await.unwrap();

    // First start should succeed
    core.start().await.unwrap();

    // Second start should fail
    let result = core.start().await;
    assert!(result.is_err(), "Start should fail when already running");
    assert!(result.unwrap_err().to_string().contains("already running"));
}

#[tokio::test]
async fn test_stop_not_running_fails() {
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

    let mut core = DrasiServerCore::new(config);
    core.initialize().await.unwrap();

    let result = core.stop().await;
    assert!(result.is_err(), "Stop should fail when not running");
    assert!(result.unwrap_err().to_string().contains("already stopped"));
}

#[tokio::test]
async fn test_start_and_stop_lifecycle() {
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

    let mut core = DrasiServerCore::new(config);
    core.initialize().await.unwrap();

    assert!(
        !core.is_running().await,
        "Server should not be running initially"
    );

    core.start().await.unwrap();
    assert!(
        core.is_running().await,
        "Server should be running after start"
    );

    core.stop().await.unwrap();
    assert!(
        !core.is_running().await,
        "Server should not be running after stop"
    );
}

#[tokio::test]
async fn test_is_running_initial_state() {
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
    assert!(
        !core.is_running().await,
        "Server should not be running initially"
    );
}

#[tokio::test]
async fn test_lifecycle_start_then_stop() {
    let core = DrasiServerCore::builder()
        .add_source(Source::mock("test-source").auto_start(false).build())
        .build()
        .await
        .unwrap();

    // Initially stopped
    let status = core.get_source_status("test-source").await.unwrap();
    assert!(matches!(status, ComponentStatus::Stopped));

    // Start
    core.start_source("test-source").await.unwrap();
    let status = core.get_source_status("test-source").await.unwrap();
    assert!(matches!(
        status,
        ComponentStatus::Starting | ComponentStatus::Running
    ));

    // Stop
    core.stop_source("test-source").await.unwrap();
    let status = core.get_source_status("test-source").await.unwrap();
    assert!(matches!(
        status,
        ComponentStatus::Stopped | ComponentStatus::Stopping
    ));
}

#[tokio::test]
async fn test_start_stop_all_component_types() {
    let core = DrasiServerCore::builder()
        .add_source(Source::mock("source1").auto_start(false).build())
        .add_query(
            Query::cypher("query1")
                .query("MATCH (n) RETURN n")
                .from_source("source1")
                .auto_start(false)
                .build(),
        )
        .add_reaction(
            Reaction::log("reaction1")
                .subscribe_to("query1")
                .auto_start(false)
                .build(),
        )
        .build()
        .await
        .unwrap();

    // All should be stopped initially
    assert!(matches!(
        core.get_source_status("source1").await.unwrap(),
        ComponentStatus::Stopped
    ));
    assert!(matches!(
        core.get_query_status("query1").await.unwrap(),
        ComponentStatus::Stopped
    ));
    assert!(matches!(
        core.get_reaction_status("reaction1").await.unwrap(),
        ComponentStatus::Stopped
    ));

    // Start all
    core.start_source("source1").await.unwrap();
    core.start_query("query1").await.unwrap();
    core.start_reaction("reaction1").await.unwrap();

    // All should be starting or running
    let source_status = core.get_source_status("source1").await.unwrap();
    assert!(matches!(
        source_status,
        ComponentStatus::Starting | ComponentStatus::Running
    ));

    let query_status = core.get_query_status("query1").await.unwrap();
    assert!(matches!(
        query_status,
        ComponentStatus::Starting | ComponentStatus::Running
    ));

    let reaction_status = core.get_reaction_status("reaction1").await.unwrap();
    assert!(matches!(
        reaction_status,
        ComponentStatus::Starting | ComponentStatus::Running
    ));
}
