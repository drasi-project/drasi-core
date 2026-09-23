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

//! One DrasiLib instance, one shared plugin source, two independent computation graphs.
//! Run: cargo run -p drasi-lib --example computation_instance

use drasi_lib::{
    computation::v1::{
        ComputationOptions, ReactionPluginHost, ReactionPluginOptions, SourceSubscriptionOptions,
    },
    DrasiLib,
};
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
    let query = drasi_lib::Query::cypher("orders-query")
        .query("MATCH (o:Order) RETURN o.name AS name")
        .from_source("orders")
        .enable_bootstrap(false)
        .build();
    let (ordinary_reaction, ordinary_handle) =
        ApplicationReaction::new("ordinary-output", vec![query.id.clone()]);
    let mut ordinary_output = ordinary_handle
        .take_receiver()
        .await
        .ok_or_else(|| anyhow::anyhow!("ordinary receiver already taken"))?;
    let drasi = DrasiLib::builder()
        .with_id("parallel-example")
        .with_source(source)
        .with_query(query.clone())
        .with_reaction(ordinary_reaction)
        .build()
        .await?;

    let pipeline = drasi.computation_pipeline("additional-orders")?;
    let (additional_reaction, additional_handle) =
        ApplicationReaction::new("additional-output", vec![query.id.clone()]);
    let mut additional_output = additional_handle
        .take_receiver()
        .await
        .ok_or_else(|| anyhow::anyhow!("additional receiver already taken"))?;
    let reaction = ReactionPluginHost::owned(
        Box::new(additional_reaction),
        pipeline.services(),
        pipeline.catalog(),
        ReactionPluginOptions::default(),
    )?;
    let graph = pipeline
        .source(
            drasi.borrow_computation_source("orders").await?,
            SourceSubscriptionOptions::default(),
        )?
        .query(query)
        .reaction(reaction, true)
        .build()?;
    let handle = drasi
        .add_computation_graph(graph, ComputationOptions::default())
        .await?;

    // The instance releases the shared source's startup fence after both paths subscribe.
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
    let (ordinary, additional) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(ordinary_output.recv(), additional_output.recv())
    })
    .await?;
    let ordinary = ordinary.ok_or_else(|| anyhow::anyhow!("ordinary output closed"))?;
    let additional = additional.ok_or_else(|| anyhow::anyhow!("additional output closed"))?;
    anyhow::ensure!(
        ordinary.results == additional.results,
        "query results differ"
    );
    println!("Both graphs produced: {:?}", additional.results);
    println!(
        "Additional graph revision: {:?}",
        handle.inspector().snapshot().desired.revision
    );

    drasi.stop_computation_graph("additional-orders").await?;
    input
        .send_node_insert(
            "two",
            vec!["Order"],
            PropertyMapBuilder::new()
                .with_string("name", "Ordinary pipeline still running")
                .build(),
        )
        .await?;
    let result = tokio::time::timeout(Duration::from_secs(5), ordinary_output.recv())
        .await?
        .ok_or_else(|| anyhow::anyhow!("ordinary output stopped"))?;
    println!(
        "After stopping only the additional graph: {:?}",
        result.results
    );
    // This transient source has no replay: events sent while the extra graph is stopped
    // are intentionally not recoverable by that subscriber.
    drasi.shutdown().await?;
    Ok(())
}
