use super::super::*;
use crate::api::{Query, Reaction, Source};

#[tokio::test]
async fn test_builder_creates_initialized_server() {
    let core = DrasiServerCore::builder()
        .with_id("builder-test")
        .build()
        .await;

    assert!(core.is_ok(), "Builder should create initialized server");
    let core = core.unwrap();
    assert!(
        core.state_guard.is_initialized().await,
        "Server should be initialized"
    );
}

#[tokio::test]
async fn test_from_config_str_creates_server() {
    let yaml = r#"
server_core:
  id: yaml-test
sources: []
queries: []
reactions: []
"#;
    let core = DrasiServerCore::from_config_str(yaml).await;
    assert!(core.is_ok(), "from_config_str should create server");
    assert!(core.unwrap().state_guard.is_initialized().await);
}

#[tokio::test]
async fn test_builder_with_components() {
    let core = DrasiServerCore::builder()
        .with_id("complex-server")
        .add_source(Source::application("source1").build())
        .add_query(
            Query::cypher("query1")
                .query("MATCH (n) RETURN n")
                .from_source("source1")
                .build(),
        )
        .add_reaction(Reaction::log("reaction1").subscribe_to("query1").build())
        .build()
        .await;

    assert!(core.is_ok(), "Builder with components should succeed");
    let core = core.unwrap();
    assert!(core.state_guard.is_initialized().await);
    assert_eq!(core.config.sources.len(), 1);
    assert_eq!(core.config.queries.len(), 1);
    assert_eq!(core.config.reactions.len(), 1);
}
