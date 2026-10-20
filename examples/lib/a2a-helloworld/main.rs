// Copyright 2026 The Drasi Authors.
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

//! HTTP source of invoices → Cypher query → A2A reaction against the
//! official Hello World agent at http://127.0.0.1:9999/

use anyhow::Result;
use std::sync::Arc;

use drasi_lib::{DrasiLib, Query};
use drasi_reaction_a2a::A2AReaction;
use drasi_reaction_log::{LogReaction, QueryConfig, TemplateSpec};
use drasi_source_http::HttpSource;
use drasi_state_store_redb::RedbStateStoreProvider;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let endpoint = std::env::var("A2A_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:9999/".to_string());

    let _state_dir = tempfile::tempdir()?;
    let state_store = RedbStateStoreProvider::new(_state_dir.path().join("state.redb"))?;

    let invoices = HttpSource::builder("invoices")
        .with_host("127.0.0.1")
        .with_port(9000)
        .build()?;

    let overdue = Query::cypher("overdue-invoices")
        .query(
            r#"
            MATCH (inv:Invoice)
            RETURN inv.invoiceId AS invoiceId,
                   inv.customer AS customer,
                   inv.amount AS amount
            "#,
        )
        .from_source("invoices")
        .auto_start(true)
        .enable_bootstrap(false)
        .build();

    let log_reaction = LogReaction::builder("console")
        .from_query("overdue-invoices")
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new(
                "[+] invoice {{after.invoiceId}} {{after.customer}} {{after.amount}}",
            )),
            updated: Some(TemplateSpec::new(
                "[~] invoice {{after.invoiceId}} {{before.amount}} -> {{after.amount}}",
            )),
            deleted: Some(TemplateSpec::new("[-] invoice {{before.invoiceId}}")),
        })
        .build()?;

    let a2a_reaction = A2AReaction::builder("helloworld-agent")
        .with_query("overdue-invoices")
        .with_endpoint(&endpoint)
        .with_result_key_fields(["invoiceId"])
        .with_instruction_template(
            "Say hello. Overdue invoice {{after.invoiceId}} for {{after.customer}} (amount {{after.amount}}).",
        )
        .build()?;

    let core = DrasiLib::builder()
        .with_id("a2a-helloworld-example")
        .with_state_store_provider(Arc::new(state_store))
        .with_source(invoices)
        .with_query(overdue)
        .with_reaction(log_reaction)
        .with_reaction(a2a_reaction)
        .build()
        .await?;

    core.start().await?;

    println!("A2A Hello World example");
    println!("  HTTP source:  http://127.0.0.1:9000/sources/invoices/events");
    println!("  A2A endpoint: {endpoint}");
    println!("  Query:        overdue-invoices (no bootstrap — insert invoices via HTTP)");
    println!();
    println!("Start Hello World first (Python 3.10+): python __main__.py");
    println!();
    println!("Insert an invoice:");
    println!(
        "  curl -X POST http://127.0.0.1:9000/sources/invoices/events -H 'Content-Type: application/json' -d '{{\"operation\":\"insert\",\"element\":{{\"type\":\"node\",\"id\":\"inv-1001\",\"labels\":[\"Invoice\"],\"properties\":{{\"invoiceId\":\"inv-1001\",\"customer\":\"Acme\",\"amount\":1250.5}}}}}}'"
    );
    println!();
    println!("Ctrl+C to stop.");

    tokio::signal::ctrl_c().await?;
    core.stop().await?;
    Ok(())
}
