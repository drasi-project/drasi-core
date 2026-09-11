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

//! The ordinary DrasiLib API, explicitly backed by a ComputationGraph.
//! Run: cargo run -p drasi-lib --features computation --example computation_runtime

use drasi_lib::{DrasiLib, ExecutionMode, Query};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use std::time::Duration;

#[tokio::main]
#[allow(clippy::print_stdout)]
async fn main() -> anyhow::Result<()> {
    let (source, input) = ApplicationSource::new(
        "orders",
        ApplicationSourceConfig {
            properties: Default::default(),
            durability: None,
        },
    )?;
    let (reaction, output) = ApplicationReaction::new("output", vec!["orders-query".into()]);
    let mut receiver = output
        .take_receiver()
        .await
        .ok_or_else(|| anyhow::anyhow!("output receiver already taken"))?;
    let drasi = DrasiLib::builder()
        .with_id("native-runtime-example")
        .with_execution_mode(ExecutionMode::ComputationGraph)
        .with_source(source)
        .with_query(
            Query::cypher("orders-query")
                .query("MATCH (o:Order) RETURN o.name AS name")
                .from_source("orders")
                .enable_bootstrap(false)
                .build(),
        )
        .with_reaction(reaction)
        .build()
        .await?;

    drasi.start().await?;
    input
        .send_node_insert(
            "one",
            vec!["Order"],
            PropertyMapBuilder::new()
                .with_string("name", "First order")
                .build(),
        )
        .await?;
    let result = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await?
        .ok_or_else(|| anyhow::anyhow!("output closed"))?;
    println!("{:?} produced {:?}", drasi.execution_mode(), result.results);
    println!(
        "Current rows: {:?}",
        drasi.get_query_results("orders-query").await?
    );
    println!(
        "Query metrics: {:?}",
        drasi.get_query_output_metrics("orders-query").await?
    );
    drasi.shutdown().await?;
    Ok(())
}
