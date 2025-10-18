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

//! Example demonstrating complete component lifecycle control
//!
//! This example shows how to:
//! - Add components with auto_start disabled
//! - Manually start and stop individual components
//! - Check component status
//! - Control component lifecycle independently

use drasi_server_core::{ComponentStatus, DrasiServerCore, Query, Reaction, Source};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    env_logger::init();

    println!("=== Component Lifecycle Control Example ===\n");

    // Create a server with components that do NOT auto-start
    let core = DrasiServerCore::builder()
        .with_id("lifecycle-example")
        .add_source(Source::mock("data-source")
                .auto_start(false) // Explicitly disabled
                .build())
        .add_query(
            Query::cypher("user-query")
                .query("MATCH (n:User) RETURN n")
                .from_source("data-source")
                .auto_start(false) // Explicitly disabled
                .build(),
        )
        .add_reaction(
            Reaction::log("logging-reaction")
                .subscribe_to("user-query")
                .auto_start(false) // Explicitly disabled
                .build(),
        )
        .build()
        .await?;

    println!("✅ Server created with components (all stopped)\n");

    // Check initial status - all should be stopped
    println!("📊 Initial Component Status:");
    print_all_statuses(&core).await?;

    // Manually start components in sequence
    println!("\n🚀 Starting components manually...\n");

    println!("1. Starting source...");
    core.start_source("data-source").await?;
    print_status(&core, "data-source", "source").await?;

    println!("\n2. Starting query...");
    core.start_query("user-query").await?;
    print_status(&core, "user-query", "query").await?;

    println!("\n3. Starting reaction...");
    core.start_reaction("logging-reaction").await?;
    print_status(&core, "logging-reaction", "reaction").await?;

    println!("\n✅ All components started!\n");

    // Check running status
    println!("📊 Running Component Status:");
    print_all_statuses(&core).await?;

    // Stop components in reverse order
    println!("\n⏸️  Stopping components manually...\n");

    println!("1. Stopping reaction...");
    core.stop_reaction("logging-reaction").await?;
    print_status(&core, "logging-reaction", "reaction").await?;

    println!("\n2. Stopping query...");
    core.stop_query("user-query").await?;
    print_status(&core, "user-query", "query").await?;

    println!("\n3. Stopping source...");
    core.stop_source("data-source").await?;
    print_status(&core, "data-source", "source").await?;

    println!("\n✅ All components stopped!\n");

    // Check final status - all should be stopped again
    println!("📊 Final Component Status:");
    print_all_statuses(&core).await?;

    println!("\n=== Lifecycle Control Example Complete ===");
    println!("\nKey Takeaways:");
    println!("  ✓ Components can be added with auto_start=false");
    println!("  ✓ Individual components can be started/stopped independently");
    println!("  ✓ Status can be checked at any time");
    println!("  ✓ Full lifecycle control without server restart");

    Ok(())
}

async fn print_all_statuses(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    let source_status = core.get_source_status("data-source").await?;
    let query_status = core.get_query_status("user-query").await?;
    let reaction_status = core.get_reaction_status("logging-reaction").await?;

    println!("  Source 'data-source': {:?}", source_status);
    println!("  Query 'user-query': {:?}", query_status);
    println!("  Reaction 'logging-reaction': {:?}", reaction_status);

    Ok(())
}

async fn print_status(
    core: &DrasiServerCore,
    id: &str,
    component_type: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let status = match component_type {
        "source" => core.get_source_status(id).await?,
        "query" => core.get_query_status(id).await?,
        "reaction" => core.get_reaction_status(id).await?,
        _ => ComponentStatus::Stopped,
    };

    let emoji = match status {
        ComponentStatus::Running => "✅",
        ComponentStatus::Stopped => "⏹️ ",
        ComponentStatus::Starting => "🟡",
        ComponentStatus::Stopping => "🟠",
        ComponentStatus::Error => "❌",
    };

    println!("   {} Status: {:?}", emoji, status);
    Ok(())
}
