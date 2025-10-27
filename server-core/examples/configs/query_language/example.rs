// Query Language Example
//
// This example demonstrates how to configure both Cypher and GQL queries
// using the builder API.

use drasi_server_core::{
    api::{Query, QueryLanguage, Reaction, Source},
    DrasiServerCore,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    env_logger::init();

    // Build the server using the fluent API
    let core = DrasiServerCore::builder()
        .with_id("query-language-example")
        .with_priority_queue_capacity(10000)
        .with_dispatch_buffer_capacity(1000)
        .add_source(
            Source::mock("source1")
                .auto_start(true)
                .with_property("data_type", "sensor")
                .with_property("interval_ms", 1000)
                .build(),
        )
        // Example 1: Cypher query (explicit)
        .add_query(
            Query::cypher("cypher-query-explicit")
                .query("MATCH (n:Person) WHERE n.age > 25 RETURN n.name, n.age")
                .from_source("source1")
                .auto_start(true)
                .build(),
        )
        // Example 2: Cypher query (default - backward compatible)
        .add_query(
            Query::new("cypher-query-default", QueryLanguage::Cypher)
                .query("MATCH (p:Person)-[:KNOWS]->(friend) RETURN p, friend")
                .from_source("source1")
                .auto_start(true)
                .build(),
        )
        // Example 3: GQL query
        .add_query(
            Query::gql("gql-query")
                .query("MATCH (n:Person) RETURN n.name, n.email")
                .from_source("source1")
                .auto_start(true)
                .build(),
        )
        // Example 4: GQL query with more complex pattern
        .add_query(
            Query::gql("gql-complex-query")
                .query(
                    "MATCH (p:Person)-[r:WORKS_AT]->(c:Company) \
                     WHERE c.industry = 'Technology' \
                     RETURN p.name AS person, c.name AS company, r.since AS startDate",
                )
                .from_source("source1")
                .auto_start(true)
                .build(),
        )
        // Example 5: Query with joins (works with both languages)
        .add_query(
            Query::cypher("query-with-joins")
                .query("MATCH (a:Order)-[r:CONTAINS]->(b:Product) RETURN a, r, b")
                .from_source("source1")
                .auto_start(true)
                .with_join(
                    "CONTAINS",
                    vec![
                        ("Order".to_string(), "product_id".to_string()),
                        ("Product".to_string(), "id".to_string()),
                    ],
                )
                .build(),
        )
        .add_reaction(
            Reaction::log("log-reaction")
                .subscribe_to("cypher-query-explicit")
                .subscribe_to("cypher-query-default")
                .subscribe_to("gql-query")
                .subscribe_to("gql-complex-query")
                .auto_start(true)
                .with_property("log_level", "info")
                .build(),
        )
        .build()
        .await?;

    println!("✓ Query language example server initialized");
    println!("  - Server ID: query-language-example");
    println!("  - Priority queue capacity: 10000");
    println!("  - Dispatch buffer capacity: 1000");
    println!("\nQueries configured:");
    println!("  1. cypher-query-explicit (Cypher - explicit)");
    println!("  2. cypher-query-default (Cypher - default)");
    println!("  3. gql-query (GQL)");
    println!("  4. gql-complex-query (GQL with complex pattern)");
    println!("  5. query-with-joins (Cypher with joins)");
    println!("\nReaction:");
    println!("  - log-reaction (subscribes to 4 queries)");

    // Start the server
    core.start().await?;
    println!("\n✓ Server started successfully");

    // Keep the server running
    println!("\nPress Ctrl+C to stop the server...");
    tokio::signal::ctrl_c().await?;

    println!("\n✓ Shutting down...");
    core.stop().await?;
    println!("✓ Server stopped");

    Ok(())
}
