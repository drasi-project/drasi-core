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

//! Full example with all plugins registered

#[cfg(feature = "full")]
use anyhow::Result;

#[cfg(feature = "full")]
use drasi_lib::plugin_core::{SourceRegistry, ReactionRegistry};

// Import all plugin registration traits
#[cfg(feature = "full")]
use drasi_plugin_postgres::SourceRegistryPostgresExt;
#[cfg(feature = "full")]
use drasi_plugin_http::SourceRegistryHttpExt;
#[cfg(feature = "full")]
use drasi_plugin_grpc::SourceRegistryGrpcExt;
#[cfg(feature = "full")]
use drasi_plugin_platform::SourceRegistryPlatformExt;
#[cfg(feature = "full")]
use drasi_plugin_application::SourceRegistryApplicationExt;
#[cfg(feature = "full")]
use drasi_plugin_mock::SourceRegistryMockExt;

#[cfg(feature = "full")]
use drasi_plugin_http_reaction::ReactionRegistryHttpExt;
#[cfg(feature = "full")]
use drasi_plugin_http_adaptive_reaction::ReactionRegistryHttpAdaptiveExt;
#[cfg(feature = "full")]
use drasi_plugin_grpc_reaction::ReactionRegistryGrpcExt;
#[cfg(feature = "full")]
use drasi_plugin_grpc_adaptive_reaction::ReactionRegistryGrpcAdaptiveExt;
#[cfg(feature = "full")]
use drasi_plugin_sse_reaction::ReactionRegistrySseExt;
#[cfg(feature = "full")]
use drasi_plugin_platform_reaction::ReactionRegistryPlatformExt;
#[cfg(feature = "full")]
use drasi_plugin_profiler_reaction::ReactionRegistryProfilerExt;
#[cfg(feature = "full")]
use drasi_plugin_application_reaction::ReactionRegistryApplicationExt;
#[cfg(feature = "full")]
use drasi_plugin_log_reaction::ReactionRegistryLogExt;

#[cfg(feature = "full")]
#[tokio::main]
async fn main() -> Result<()> {
    println!("=== Drasi Plugin Architecture - Full Example ===");
    println!("With ALL plugins registered\n");

    // All source plugins are available through extension traits
    println!("Available Source Plugins:");
    println!("• PostgreSQL - SourceConfig::postgres()");
    println!("• HTTP - SourceConfig::http()");
    println!("• gRPC - SourceConfig::grpc()");
    println!("• Platform - SourceConfig::platform()");
    println!("• Application - SourceConfig::application()");
    println!("• Mock (core) - SourceFactory::mock()\n");

    // All reaction plugins are available
    println!("Available Reaction Plugins:");
    println!("• HTTP - ReactionConfig::http()");
    println!("• HTTP Adaptive - ReactionConfig::http_adaptive()");
    println!("• gRPC - ReactionConfig::grpc()");
    println!("• gRPC Adaptive - ReactionConfig::grpc_adaptive()");
    println!("• SSE - ReactionConfig::sse()");
    println!("• Platform - ReactionConfig::platform()");
    println!("• Profiler - ReactionConfig::profiler()");
    println!("• Application - ReactionConfig::application()");
    println!("• Log (core) - ReactionFactory::log()\n");

    // All bootstrap providers are available
    println!("Available Bootstrap Providers:");
    println!("• PostgreSQL - BootstrapProviderConfig::postgres()");
    println!("• Platform - BootstrapProviderConfig::platform()");
    println!("• ScriptFile - BootstrapProviderConfig::scriptfile()");
    println!("• Application - BootstrapProviderConfig::application()\n");

    // Example: Create a PostgreSQL source with fluent API
    let postgres_config = SourceConfig::postgres()
        .with_host("localhost")
        .with_port(5432)
        .with_database("mydb")
        .with_user("postgres")
        .with_password("password")
        .build();
    println!("✓ Created PostgreSQL source config with fluent API");

    // Example: Create HTTP reaction with fluent API
    let http_reaction = ReactionConfig::http()
        .with_base_url("http://api.example.com")
        .with_timeout_ms(5000)
        .build();
    println!("✓ Created HTTP reaction config with fluent API");

    // Example: Create ScriptFile bootstrap provider
    let bootstrap = BootstrapProviderConfig::scriptfile()
        .add_file_path("/data/bootstrap1.jsonl")
        .add_file_path("/data/bootstrap2.jsonl")
        .build();
    println!("✓ Created ScriptFile bootstrap config with fluent API");

    println!("\n=== Full Build Features ===");
    println!("• All plugins available with single import");
    println!("• Convenient prelude module");
    println!("• Type-safe fluent API for all components");
    println!("• ~18MB binary size with all features");

    Ok(())
}

#[cfg(not(feature = "full"))]
fn main() {
    println!("This example requires the 'full' feature.");
    println!("Run with: cargo run --bin full --features full");
}