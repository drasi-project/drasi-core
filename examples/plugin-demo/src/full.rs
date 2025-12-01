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

//! Full example showing all available plugins in the instance-based architecture.
//!
//! This example demonstrates how to create instances of various plugin types
//! and shows the available plugin ecosystem.
//!
//! Run with: cargo run --bin full --features full

use anyhow::Result;

fn main() -> Result<()> {
    println!("=== Drasi Instance-Based Plugin Architecture - Full Plugin List ===\n");

    println!("In the instance-based architecture, YOU create plugin instances.");
    println!("There are no registries or factories - you import and instantiate directly.\n");

    println!("=== Available Source Plugins ===\n");
    println!("  drasi-source-mock          - Mock data generator for testing");
    println!("    Usage: MockSource::new(config, event_tx)");
    println!();
    println!("  drasi-source-postgres      - PostgreSQL WAL replication");
    println!("    Usage: PostgresSource::new(config, event_tx)");
    println!();
    println!("  drasi-source-http          - HTTP endpoint polling");
    println!("    Usage: HttpSource::new(config, event_tx)");
    println!();
    println!("  drasi-source-grpc          - gRPC streaming");
    println!("    Usage: GrpcSource::new(config, event_tx)");
    println!();
    println!("  drasi-source-platform      - Redis Streams integration");
    println!("    Usage: PlatformSource::new(config, event_tx)");
    println!();
    println!("  drasi-source-application   - Programmatic API source");
    println!("    Usage: ApplicationSource::new(config, event_tx)");
    println!();

    println!("=== Available Reaction Plugins ===\n");
    println!("  drasi-reaction-log          - Console logging");
    println!("    Usage: LogReaction::new(config, event_tx)");
    println!();
    println!("  drasi-reaction-http         - HTTP webhook");
    println!("    Usage: HttpReaction::new(config, event_tx)");
    println!();
    println!("  drasi-reaction-http-adaptive - Adaptive HTTP batching");
    println!("    Usage: HttpAdaptiveReaction::new(config, event_tx)");
    println!();
    println!("  drasi-reaction-grpc         - gRPC streaming");
    println!("    Usage: GrpcReaction::new(config, event_tx)");
    println!();
    println!("  drasi-reaction-grpc-adaptive - Adaptive gRPC batching");
    println!("    Usage: GrpcAdaptiveReaction::new(config, event_tx)");
    println!();
    println!("  drasi-reaction-sse          - Server-Sent Events");
    println!("    Usage: SseReaction::new(config, event_tx)");
    println!();
    println!("  drasi-reaction-platform     - Platform integration");
    println!("    Usage: PlatformReaction::new(config, event_tx)");
    println!();
    println!("  drasi-reaction-profiler     - Performance profiling");
    println!("    Usage: ProfilerReaction::new(config, event_tx)");
    println!();
    println!("  drasi-reaction-application  - Programmatic API reaction");
    println!("    Usage: ApplicationReaction::new(config, event_tx)");
    println!();

    println!("=== Available Bootstrap Plugins ===\n");
    println!("  drasi-plugin-postgres-bootstrap    - PostgreSQL snapshot");
    println!("  drasi-plugin-platform-bootstrap    - Platform query API");
    println!("  drasi-plugin-scriptfile-bootstrap  - JSONL file loading");
    println!("  drasi-plugin-application-bootstrap - Programmatic bootstrap");
    println!("  drasi-plugin-noop-bootstrap        - No-op (empty bootstrap)");
    println!();

    println!("=== Instance-Based Architecture Pattern ===\n");
    println!("```rust");
    println!("// 1. Import the plugin type");
    println!("use drasi_source_mock::MockSource;");
    println!();
    println!("// 2. Create configuration");
    println!("let config = MockSourceConfig {{");
    println!("    data_type: \"counter\".to_string(),");
    println!("    interval_ms: 1000,");
    println!("}};");
    println!();
    println!("// 3. Create the instance (no event_tx needed - DrasiLib injects it)");
    println!("let source = MockSource::new(\"my-source\", config)?;");
    println!();
    println!("// 4. Add to DrasiLib (ownership is transferred)");
    println!("drasi.add_source(source).await?;");
    println!("```");
    println!();

    println!("=== Benefits ===");
    println!("• No registry boilerplate");
    println!("• Compile-time type safety");
    println!("• Easy dependency injection");
    println!("• Simple testing with mocks");
    println!("• Clear ownership semantics");

    Ok(())
}
