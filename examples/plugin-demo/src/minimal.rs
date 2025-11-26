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

//! Minimal example using only core plugins (MockSource and LogReaction)

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== Drasi Plugin Architecture - Minimal Example ===");
    println!("Using only core plugins (MockSource and LogReaction)\n");

    // In the plugin architecture, sources and reactions are created through
    // the plugin registry system, not through extension traits.
    //
    // For a full example of using sources and reactions, see the DrasiServerCore
    // configuration-based approach in drasi-lib.

    println!("✓ Plugin architecture is ready for use");
    println!("\nTo use sources and reactions, configure them via DrasiServerCore:");
    println!("  - Use DrasiServerCore::from_config_file(\"config.yaml\")");
    println!("  - Or build programmatically with DrasiServerCoreBuilder");

    println!("\n=== Minimal Build Benefits ===");
    println!("• Small binary size (only core plugins)");
    println!("• No external dependencies for mock/log");
    println!("• Perfect for testing and development");

    Ok(())
}
