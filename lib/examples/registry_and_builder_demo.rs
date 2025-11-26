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

//! Simple example demonstrating the Registry + Builder pattern in drasi-lib
//!
//! This example shows how the current plugin architecture works:
//!
//! 1. **Registries** - Store factory functions that create sources/reactions from configs
//! 2. **Extension Traits** - Plugins provide traits that register themselves into registries
//! 3. **Builders** - Fluent API for creating component configurations
//!
//! The flow is:
//! ```text
//! Plugin provides Extension Trait → registers factory into Registry
//! Builder creates Config → Registry uses factory to create instance
//! ```
//!
//! Run with: cargo run --example registry_and_builder_demo

use anyhow::Result;

// Core library types
use drasi_lib::{DrasiLib, Source, Query, Reaction};
use drasi_lib::plugin_core::{SourceRegistry, ReactionRegistry};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    env_logger::init();

    println!("=== Registry + Builder Pattern Demo ===\n");

    // =========================================================================
    // STEP 1: Understanding Registries
    // =========================================================================
    //
    // Registries are HashMaps that map source/reaction type strings to factory
    // functions. Without registries, you'd need hard-coded match statements.
    //
    // The registry enables:
    // - YAML config files to use string type names like `source_type: mock`
    // - Dynamic plugin loading at compile-time via feature flags
    // - Third-party plugins to register themselves

    println!("Step 1: Understanding Registries\n");
    println!("  Registries map type strings -> factory functions");
    println!("  Example: 'mock' -> |config, event_tx| MockSource::new(config, event_tx)\n");

    // Create empty registries
    let source_registry = SourceRegistry::new();
    let reaction_registry = ReactionRegistry::new();

    println!("  Empty source registry types: {:?}", source_registry.registered_types());
    println!("  Empty reaction registry types: {:?}", reaction_registry.registered_types());

    // In a real application, you would register plugins like this:
    //
    // use drasi_plugin_mock::SourceRegistryMockExt;
    // source_registry.register_mock();  // Adds "mock" factory
    //
    // Each plugin provides an extension trait that adds a register_xxx() method:
    //
    //   pub trait SourceRegistryMockExt {
    //       fn register_mock(&mut self);
    //   }
    //   impl SourceRegistryMockExt for SourceRegistry {
    //       fn register_mock(&mut self) {
    //           self.register("mock".to_string(), |config, event_tx| {
    //               Ok(Arc::new(MockSource::new(config, event_tx)?))
    //           });
    //       }
    //   }

    // =========================================================================
    // STEP 2: Using Builders to Create Configurations
    // =========================================================================
    //
    // Builders provide a fluent API for creating SourceConfig, QueryConfig,
    // and ReactionConfig structs. These configs are just data - they don't
    // contain the actual source/reaction instances yet.

    println!("\nStep 2: Using Builders to Create Configurations\n");

    // Create a mock source config using the builder
    let source_config = Source::mock("demo-source")
        .auto_start(true)
        .build();

    println!("  Created SourceConfig:");
    println!("    id: {}", source_config.id);
    println!("    type: {}", source_config.source_type());
    println!("    auto_start: {}", source_config.auto_start);

    // Create a query config
    let query_config = Query::cypher("demo-query")
        .query("MATCH (n:Person) RETURN n.name, n.age")
        .from_source("demo-source")
        .auto_start(true)
        .build();

    println!("\n  Created QueryConfig:");
    println!("    id: {}", query_config.id);
    println!("    query: {}", query_config.query);

    // Create a log reaction config
    let reaction_config = Reaction::log("demo-reaction")
        .subscribe_to("demo-query")
        .auto_start(true)
        .build();

    println!("\n  Created ReactionConfig:");
    println!("    id: {}", reaction_config.id);
    println!("    type: {}", reaction_config.reaction_type());

    // =========================================================================
    // STEP 3: How DrasiLibBuilder Uses Registries
    // =========================================================================
    //
    // DrasiLibBuilder accepts:
    // - Custom registries (with plugins registered)
    // - Component configurations (created via builders)
    //
    // During build(), it uses the registries to instantiate actual source/reaction
    // objects from the configs.

    println!("\nStep 3: How DrasiLibBuilder Uses Registries\n");
    println!("  DrasiLib::builder()");
    println!("      .with_source_registry(registry)    // Pass registry with plugins");
    println!("      .with_reaction_registry(registry)");
    println!("      .add_source(source_config)          // Add configs");
    println!("      .add_query(query_config)");
    println!("      .add_reaction(reaction_config)");
    println!("      .build().await                      // Registry creates instances");

    // Note: We're not actually building here because we don't have plugins
    // registered. In a real application, you would:
    //
    // let core = DrasiLib::builder()
    //     .with_id("demo-server")
    //     .with_source_registry(source_registry)
    //     .with_reaction_registry(reaction_registry)
    //     .add_source(source_config)
    //     .add_query(query_config)
    //     .add_reaction(reaction_config)
    //     .build()
    //     .await?;

    // =========================================================================
    // STEP 4: Alternative - Config File Approach
    // =========================================================================
    //
    // You can also use YAML configuration files instead of the builder API.
    // The registry still maps type strings to factories.

    println!("\nStep 4: Alternative - YAML Config File Approach\n");

    let config_yaml = r#"
server_core:
  id: demo-server

sources:
  - id: demo-source
    source_type: mock       # <-- Registry looks up "mock" factory
    auto_start: true
    properties:
      data_type: counter

queries:
  - id: demo-query
    query: "MATCH (n:Person) RETURN n.name"
    sources: [demo-source]

reactions:
  - id: demo-reaction
    reaction_type: log      # <-- Registry looks up "log" factory
    queries: [demo-query]
"#;

    println!("{}", config_yaml);
    println!("  The registry maps:");
    println!("    'mock' -> MockSource factory");
    println!("    'log'  -> LogReaction factory");

    // =========================================================================
    // SUMMARY
    // =========================================================================

    println!("\n=== Summary ===\n");
    println!("The Registry + Builder pattern in drasi-lib:\n");
    println!("1. REGISTRIES map type strings to factory functions");
    println!("   - Enables YAML configs with `source_type: mock`");
    println!("   - No hard-coded match statements needed");
    println!();
    println!("2. EXTENSION TRAITS let plugins register themselves");
    println!("   - `source_registry.register_mock()` adds 'mock' factory");
    println!("   - Import trait, call register method - that's it");
    println!();
    println!("3. BUILDERS create configuration structs");
    println!("   - `Source::mock('id').auto_start(true).build()`");
    println!("   - Fluent API, type-safe, ergonomic");
    println!();
    println!("4. BUILD combines them:");
    println!("   - Registry looks up factory by config's type string");
    println!("   - Factory creates instance from config");
    println!();
    println!("This pattern was added to support YAML configuration files,");
    println!("which is why registries exist even though the original plan");
    println!("only mentioned extension traits.");

    Ok(())
}
