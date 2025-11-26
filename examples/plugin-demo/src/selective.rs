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

//! Selective example using only specific plugins

#[cfg(feature = "selective")]
use anyhow::Result;
#[cfg(feature = "selective")]
use drasi_lib::config::{SourceConfig, ReactionConfig};
#[cfg(feature = "selective")]
use drasi_lib::bootstrap::BootstrapProviderConfig;

// Import only the plugins we need
#[cfg(feature = "selective")]
use drasi_plugin_postgres::SourceConfigPostgresExt;
#[cfg(feature = "selective")]
use drasi_plugin_http_reaction::ReactionConfigHttpExt;
#[cfg(feature = "selective")]
use drasi_plugin_scriptfile_bootstrap::BootstrapProviderConfigScriptFileExt;

#[cfg(feature = "selective")]
#[tokio::main]
async fn main() -> Result<()> {
    println!("=== Drasi Plugin Architecture - Selective Example ===");
    println!("Using only PostgreSQL source, HTTP reaction, and ScriptFile bootstrap\n");

    // Create PostgreSQL source config
    let postgres_source = SourceConfig::postgres()
        .with_host("localhost")
        .with_port(5432)
        .with_database("mydb")
        .with_user("postgres")
        .with_password("secret")
        .with_tables(vec!["users", "orders"])
        .build();

    println!("✓ Created PostgreSQL source configuration");
    println!("  Database: mydb");
    println!("  Tables: users, orders");

    // Create HTTP reaction config
    let http_reaction = ReactionConfig::http()
        .with_base_url("https://webhook.site/unique-id")
        .with_token("Bearer abc123")
        .with_timeout_ms(10000)
        .build();

    println!("\n✓ Created HTTP reaction configuration");
    println!("  Endpoint: https://webhook.site/unique-id");
    println!("  Timeout: 10s");

    // Create ScriptFile bootstrap provider
    let bootstrap_provider = BootstrapProviderConfig::scriptfile()
        .add_file_path("/data/initial_users.jsonl")
        .add_file_path("/data/initial_orders.jsonl")
        .build();

    println!("\n✓ Created ScriptFile bootstrap provider");
    println!("  Files: initial_users.jsonl, initial_orders.jsonl");

    println!("\n=== Selective Build Benefits ===");
    println!("• Only include plugins you actually use");
    println!("• Reduced binary size vs full build");
    println!("• Clear dependencies in Cargo.toml");
    println!("• ~10MB binary size (between minimal and full)");

    println!("\nPlugins NOT included in this build:");
    println!("✗ HTTP, gRPC, Platform, Application sources");
    println!("✗ gRPC, SSE, Platform, Profiler reactions");
    println!("✗ PostgreSQL, Platform, Application bootstrap providers");

    Ok(())
}

#[cfg(not(feature = "selective"))]
fn main() {
    println!("This example requires the 'selective' feature.");
    println!("Run with: cargo run --bin selective --features selective");
}