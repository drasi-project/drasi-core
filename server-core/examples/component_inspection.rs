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

//! Example demonstrating the component listing and inspection API
//!
//! This example shows how to:
//! - List all sources, queries, and reactions
//! - Get detailed information about specific components
//! - Check component status
//! - Add and remove components at runtime and observe changes

use drasi_server_core::{DrasiServerCore, Query, Reaction, Source};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    env_logger::init();

    println!("=== Component Inspection Example ===\n");

    // Create a server with some initial components
    let core = DrasiServerCore::builder()
        .with_id("inspection-example")
        .add_source(Source::application("data-source-1").build())
        .add_source(Source::mock("data-source-2").build())
        .add_query(
            Query::cypher("user-query")
                .query("MATCH (n:User) RETURN n")
                .from_source("data-source-1")
                .build(),
        )
        .add_reaction(
            Reaction::log("logging-reaction")
                .subscribe_to("user-query")
                .build(),
        )
        .build()
        .await?;

    // List all sources
    println!("📦 Sources:");
    let sources = core.list_sources().await?;
    for (id, status) in &sources {
        println!("  - {} [{:?}]", id, status);
    }
    println!();

    // List all queries
    println!("🔍 Queries:");
    let queries = core.list_queries().await?;
    for (id, status) in &queries {
        println!("  - {} [{:?}]", id, status);
    }
    println!();

    // List all reactions
    println!("⚡ Reactions:");
    let reactions = core.list_reactions().await?;
    for (id, status) in &reactions {
        println!("  - {} [{:?}]", id, status);
    }
    println!();

    // Get detailed information about a specific source
    println!("📊 Detailed Source Information:");
    let source_info = core.get_source_info("data-source-1").await?;
    println!("  ID: {}", source_info.id);
    println!("  Type: {}", source_info.source_type);
    println!("  Status: {:?}", source_info.status);
    if let Some(error) = source_info.error_message {
        println!("  Error: {}", error);
    }
    println!();

    // Get detailed information about a specific query
    println!("📊 Detailed Query Information:");
    let query_info = core.get_query_info("user-query").await?;
    println!("  ID: {}", query_info.id);
    println!("  Query: {}", query_info.query);
    println!("  Status: {:?}", query_info.status);
    println!("  Sources: {:?}", query_info.sources);
    println!();

    // Get detailed information about a specific reaction
    println!("📊 Detailed Reaction Information:");
    let reaction_info = core.get_reaction_info("logging-reaction").await?;
    println!("  ID: {}", reaction_info.id);
    println!("  Type: {}", reaction_info.reaction_type);
    println!("  Status: {:?}", reaction_info.status);
    println!("  Subscribed Queries: {:?}", reaction_info.queries);
    println!();

    // Demonstrate runtime addition
    println!("➕ Adding a new source at runtime...");
    core.add_source_runtime(Source::application("runtime-source").build())
        .await?;

    let sources = core.list_sources().await?;
    println!("  Total sources after addition: {}", sources.len());
    println!();

    // Demonstrate runtime removal
    println!("➖ Removing a source at runtime...");
    core.remove_source("data-source-2").await?;

    let sources = core.list_sources().await?;
    println!("  Total sources after removal: {}", sources.len());
    for (id, status) in &sources {
        println!("  - {} [{:?}]", id, status);
    }
    println!();

    // Check status of individual components
    println!("🔍 Checking component statuses:");
    let source_status = core.get_source_status("data-source-1").await?;
    println!("  data-source-1: {:?}", source_status);

    let query_status = core.get_query_status("user-query").await?;
    println!("  user-query: {:?}", query_status);

    let reaction_status = core.get_reaction_status("logging-reaction").await?;
    println!("  logging-reaction: {:?}", reaction_status);
    println!();

    println!("✅ Example completed successfully!");

    Ok(())
}
