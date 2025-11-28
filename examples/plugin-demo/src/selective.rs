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
    println!("use drasi_lib::channels::ComponentEventSender;");
    println!("use drasi_lib::config::*;");
    println!("use std::sync::Arc;");
    println!("use std::collections::HashMap;");
    println!();
    println!("// Import only the plugins you need");
    println!("use drasi_plugin_postgres::PostgresSource;");
    println!("use drasi_plugin_http_reaction::HttpReaction;");
    println!();
    println!("#[tokio::main]");
    println!("async fn main() -> Result<()> {{");
    println!("    // 1. Build DrasiLib with queries");
    println!("    let mut drasi = DrasiLib::builder()");
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
    println!("    // 2. Create event channel");
    println!("    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(100);");
    println!();
    println!("    // 3. Create PostgreSQL source");
    println!("    let pg_config = SourceConfig {{");
    println!("        id: \"postgres-source\".to_string(),");
    println!("        auto_start: true,");
    println!("        config: SourceSpecificConfig::Postgres({{");
    println!("            let mut props = HashMap::new();");
    println!("            props.insert(\"host\".into(), json!(\"localhost\"));");
    println!("            props.insert(\"port\".into(), json!(5432));");
    println!("            props.insert(\"database\".into(), json!(\"mydb\"));");
    println!("            props.insert(\"user\".into(), json!(\"postgres\"));");
    println!("            props.insert(\"password\".into(), json!(\"secret\"));");
    println!("            props.insert(\"tables\".into(), json!([\"users\"]));");
    println!("            props");
    println!("        }}),");
    println!("        bootstrap_provider: Some(BootstrapProviderConfig {{");
    println!("            provider_type: \"scriptfile\".to_string(),");
    println!("            config: {{");
    println!("                let mut props = HashMap::new();");
    println!("                props.insert(\"file_paths\".into(), ");
    println!("                    json!([\"/data/initial_users.jsonl\"]));");
    println!("                props");
    println!("            }},");
    println!("        }}),");
    println!("        ..Default::default()");
    println!("    }};");
    println!("    let postgres_source = Arc::new(PostgresSource::new(pg_config, event_tx.clone())?);");
    println!("    drasi.add_source(postgres_source).await?;");
    println!();
    println!("    // 4. Create HTTP reaction");
    println!("    let http_config = ReactionConfig {{");
    println!("        id: \"http-reaction\".to_string(),");
    println!("        queries: vec![\"user-query\".to_string()],");
    println!("        auto_start: true,");
    println!("        config: ReactionSpecificConfig::Http({{");
    println!("            let mut props = HashMap::new();");
    println!("            props.insert(\"base_url\".into(), ");
    println!("                json!(\"https://api.example.com/webhook\"));");
    println!("            props.insert(\"timeout_ms\".into(), json!(5000));");
    println!("            props");
    println!("        }}),");
    println!("        ..Default::default()");
    println!("    }};");
    println!("    let http_reaction = Arc::new(HttpReaction::new(http_config, event_tx)?);");
    println!("    drasi.add_reaction(http_reaction).await?;");
    println!();
    println!("    // 5. Start everything");
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
