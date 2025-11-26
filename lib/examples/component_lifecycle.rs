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

//! Example demonstrating component lifecycle management
//!
//! This example shows how to:
//! - Create and initialize a DrasiLib
//! - Start and stop components
//!
//! In the plugin architecture, components are created via configuration.

use drasi_lib::server_core::DrasiLib;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    env_logger::init();

    println!("=== Component Lifecycle Example ===\n");

    // Create a server from configuration string
    let config_yaml = r#"
server_core:
  id: lifecycle-example

sources: []
queries: []
reactions: []
"#;

    let core = DrasiLib::from_config_str(config_yaml).await?;
    core.start().await?;

    println!("DrasiLib initialized");
    println!("\nIn the plugin architecture:");
    println!("- Components are defined in YAML configuration files");
    println!("- Use auto_start: true to start components automatically");
    println!("- Or call start_source(), start_query(), start_reaction() programmatically");

    Ok(())
}
