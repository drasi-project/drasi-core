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
//!
//! In the plugin architecture, components are created via configuration
//! and the DrasiLib.

use drasi_lib::server_core::DrasiLib;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    env_logger::init();

    println!("=== Component Inspection Example ===\n");

    // Create a server from configuration string
    let config_yaml = r#"
server_core:
  id: inspection-example

sources: []
queries: []
reactions: []
"#;

    let core = DrasiLib::from_config_str(config_yaml).await?;
    core.start().await?;

    println!("DrasiLib initialized");
    println!("\nIn the plugin architecture, components are created via YAML configuration.");
    println!("See the config/ directory for example configurations.");

    // List components
    println!("\nListing components:");
    let sources = core.list_sources().await?;
    println!("  Sources: {:?}", sources);

    let queries = core.list_queries().await?;
    println!("  Queries: {:?}", queries);

    let reactions = core.list_reactions().await?;
    println!("  Reactions: {:?}", reactions);

    Ok(())
}
