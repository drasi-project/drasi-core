use anyhow::Result;
use drasi_bootstrap_neo4j::{Neo4jBootstrapConfig, Neo4jBootstrapProvider};
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_log::{LogReaction, QueryConfig, TemplateSpec};
use drasi_source_neo4j::Neo4jSource;
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let uri = std::env::var("NEO4J_URI").unwrap_or_else(|_| "bolt://localhost:7687".to_string());
    let user = std::env::var("NEO4J_USER").unwrap_or_else(|_| "neo4j".to_string());
    let password = std::env::var("NEO4J_PASSWORD").unwrap_or_else(|_| "testpassword".to_string());
    let database = std::env::var("NEO4J_DATABASE").unwrap_or_else(|_| "neo4j".to_string());

    let bootstrap = Neo4jBootstrapProvider::new(Neo4jBootstrapConfig {
        uri: uri.clone(),
        user: user.clone(),
        password: password.clone(),
        database: database.clone(),
        labels: vec!["Person".to_string(), "Movie".to_string()],
        rel_types: vec!["ACTED_IN".to_string()],
    });

    let source = Neo4jSource::builder("neo4j-source")
        .with_uri(uri)
        .with_user(user)
        .with_password(password)
        .with_database(database)
        .with_labels(vec!["Person".to_string(), "Movie".to_string()])
        .with_rel_types(vec!["ACTED_IN".to_string()])
        .with_poll_interval(Duration::from_millis(250))
        .with_bootstrap_provider(bootstrap)
        .start_from_now()
        .build()?;

    let query = Query::cypher("actors")
        .query(
            r#"
            MATCH (p:Person)-[r:ACTED_IN]->(m:Movie)
            RETURN p.name AS actor, m.title AS movie, r.role AS role
            "#,
        )
        .from_source("neo4j-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let reaction = LogReaction::builder("console")
        .from_query("actors")
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new("[+] {{after.actor}} acted in {{after.movie}} as {{after.role}}")),
            updated: Some(TemplateSpec::new("[~] {{after.actor}} acted in {{after.movie}} as {{after.role}}")),
            deleted: Some(TemplateSpec::new("[-] {{before.actor}} / {{before.movie}} removed")),
        })
        .build()?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("neo4j-example")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await?,
    );

    core.start().await?;
    println!("Neo4j example running. Press Ctrl+C to exit.");
    tokio::signal::ctrl_c().await?;
    core.stop().await?;
    Ok(())
}
