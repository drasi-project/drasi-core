// Copyright 2024 The Drasi Authors.
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

//! Example demonstrating the plugin system for extensible middleware.
//!
//! This example shows how to:
//! - Register built-in middleware factories
//! - Configure and register a plugin loader
//! - Discover and load middleware plugins from a directory
//! - Use loaded middleware in a continuous query
//!
//! Note: This example demonstrates the infrastructure. Actual plugin loading
//! will require format-specific loaders (WASM, dynamic library) to be implemented.

use drasi_core::middleware::{
    fs_plugin_loader::FileSystemPluginLoader, plugin_loader::PluginLoaderConfig,
    MiddlewareTypeRegistry,
};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    env_logger::init();

    println!("=== Drasi Plugin System Example ===\n");

    // Create a middleware type registry
    let mut registry = MiddlewareTypeRegistry::new();

    println!("1. Registering built-in middleware...");
    // In a real application, you would register your built-in middleware here:
    // registry.register(Arc::new(MapFactory::new()));
    // registry.register(Arc::new(DecoderFactory::new()));
    println!("   Built-in middleware registered: {}", registry.count());

    println!("\n2. Configuring plugin loader...");
    // Configure the file system plugin loader
    let plugin_dir = PathBuf::from("./plugins");
    let config = PluginLoaderConfig {
        plugin_dir: plugin_dir.clone(),
        recursive: false,
        fail_on_error: false,
        ..Default::default()
    };

    println!("   Plugin directory: {}", plugin_dir.display());
    println!("   Recursive scanning: {}", config.recursive);
    println!("   Fail on error: {}", config.fail_on_error);

    // Create and register the plugin loader
    let loader = FileSystemPluginLoader::new(config);
    registry.register_plugin_loader(Box::new(loader));
    println!("   Plugin loader registered");

    println!("\n3. Discovering and loading plugins...");
    match registry.discover_and_load_plugins() {
        Ok(count) => {
            println!("   Successfully loaded {} plugin(s)", count);
            if count == 0 {
                println!("   Note: No plugins found in {}", plugin_dir.display());
                println!("   This is expected if the plugins directory doesn't exist");
                println!("   or contains no .wasm files.");
            }
        }
        Err(e) => {
            println!("   Plugin loading completed with warnings: {}", e);
            println!("   This is expected as plugin format loaders are not yet implemented.");
        }
    }

    println!("\n4. Listing available middleware...");
    let middleware_names = registry.list_middleware_names();
    if middleware_names.is_empty() {
        println!("   No middleware registered");
    } else {
        for name in middleware_names {
            println!("   - {}", name);
        }
    }

    println!("\n5. Using middleware in a query (conceptual)...");
    println!("   In a real application, you would:");
    println!("   - Create a MiddlewareContainer from the registry");
    println!("   - Build a SourceMiddlewarePipeline with your middleware chain");
    println!("   - Process SourceChanges through the pipeline");
    println!("   - Feed results into your continuous query");

    println!("\n=== Example Complete ===");
    println!("\nNext steps to enable plugin loading:");
    println!("1. Implement WASM plugin loader (recommended)");
    println!("2. Create drasi-plugin-api crate with stable interfaces");
    println!("3. Develop example plugins");
    println!("4. Add plugin development documentation");
    println!("\nSee docs/PLUGIN_SYSTEM.md for more information.");

    Ok(())
}
