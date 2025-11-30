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

//! Selective example showing how to use specific plugins.
//!
//! This example demonstrates the instance-based plugin architecture
//! using only PostgreSQL source, HTTP reaction, and ScriptFile bootstrap.
//!
//! Run with: cargo run --bin selective --features selective

use anyhow::Result;

fn main() -> Result<()> {
    println!("=== Drasi Instance-Based Plugin Architecture - Selective Example ===\n");

    println!("This example shows how to use specific plugins:");
    println!("• PostgreSQL source (drasi-plugin-postgres)");
    println!("• HTTP reaction (drasi-plugin-http-reaction)");
    println!("• ScriptFile bootstrap (drasi-plugin-scriptfile-bootstrap)\n");

    println!("=== Example Code ===\n");

    println!("```rust");
    println!("use drasi_lib::{{DrasiLib, api::Query}};");
    println!();
    println!("// Import only the plugins you need");
    println!("use drasi_plugin_postgres::{{PostgresSource, PostgresSourceConfig}};");
    println!("use drasi_plugin_http_reaction::{{HttpReaction, HttpReactionConfig}};");
    println!();
    println!("#[tokio::main]");
    println!("async fn main() -> Result<()> {{");
    println!("    // 1. Build DrasiLib with queries");
    println!("    let drasi = DrasiLib::builder()");
    println!("        .with_id(\"selective-demo\")");
    println!("        .add_query(");
    println!("            Query::cypher(\"user-query\")");
    println!("                .query(\"MATCH (u:User) WHERE u.active = true RETURN u\")");
    println!("                .from_source(\"postgres-source\")");
    println!("                .build(),");
    println!("        )");
    println!("        .build()");
    println!("        .await?;");
    println!();
    println!("    // 2. Create PostgreSQL source (no event_tx needed - DrasiLib injects it)");
    println!("    let pg_config = PostgresSourceConfig {{");
    println!("        host: \"localhost\".to_string(),");
    println!("        port: 5432,");
    println!("        database: \"mydb\".to_string(),");
    println!("        user: \"postgres\".to_string(),");
    println!("        password: \"secret\".to_string(),");
    println!("        tables: vec![\"users\".to_string()],");
    println!("        ..Default::default()");
    println!("    }};");
    println!("    let postgres_source = PostgresSource::new(\"postgres-source\", pg_config)?;");
    println!("    drasi.add_source(postgres_source).await?;  // Ownership transferred");
    println!();
    println!("    // 3. Create HTTP reaction (no event_tx needed - DrasiLib injects it)");
    println!("    let http_config = HttpReactionConfig {{");
    println!("        base_url: \"https://api.example.com/webhook\".to_string(),");
    println!("        timeout_ms: 5000,");
    println!("        ..Default::default()");
    println!("    }};");
    println!("    let http_reaction = HttpReaction::new(");
    println!("        \"http-reaction\",");
    println!("        vec![\"user-query\".to_string()],");
    println!("        http_config,");
    println!("    );");
    println!("    drasi.add_reaction(http_reaction).await?;  // Ownership transferred");
    println!();
    println!("    // 4. Start everything");
    println!("    drasi.start().await?;");
    println!();
    println!("    Ok(())");
    println!("}}");
    println!("```");
    println!();

    println!("=== Selective Build Benefits ===\n");
    println!("• Smaller binary size (~10MB vs ~18MB for full build)");
    println!("• Faster compilation");
    println!("• Only include dependencies you actually use");
    println!("• Clear dependency documentation in Cargo.toml");
    println!();

    println!("=== Cargo.toml Configuration ===\n");
    println!("```toml");
    println!("[dependencies]");
    println!("drasi-lib = {{ path = \"../../lib\" }}");
    println!("drasi-plugin-postgres = {{ path = \"../../components/sources/plugin-postgres\" }}");
    println!("drasi-plugin-http-reaction = {{ path = \"../../components/reactions/plugin-http\" }}");
    println!("drasi-plugin-scriptfile-bootstrap = {{ ");
    println!("    path = \"../../components/bootstrappers/plugin-scriptfile\" ");
    println!("}}");
    println!("```");

    Ok(())
}
