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

//! One DrasiLib instance, one shared legacy source, two independent query engines.
//! Run: cargo run -p drasi-lib --features computation --example computation_instance

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
    let (legacy_reaction, legacy_handle) =
        ApplicationReaction::new("legacy-output", vec![query.id.clone()]);
    let mut legacy_output = legacy_handle
        .take_receiver()
        .await
        .ok_or_else(|| anyhow::anyhow!("legacy receiver already taken"))?;
    let drasi = DrasiLib::builder()
        .with_id("parallel-example")
        .with_source(source)
        .with_query(query.clone())
        .with_reaction(legacy_reaction)
        .build()
        .await?;

    let pipeline = drasi.computation_pipeline("native-orders")?;
    let (native_reaction, native_handle) =
        ApplicationReaction::new("native-output", vec![query.id.clone()]);
    let mut native_output = native_handle
        .take_receiver()
        .await
        .ok_or_else(|| anyhow::anyhow!("native receiver already taken"))?;
    let reaction = ReactionPluginHost::owned(
        Box::new(native_reaction),
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
    let (legacy, native) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(legacy_output.recv(), native_output.recv())
    })
    .await?;
    let legacy = legacy.ok_or_else(|| anyhow::anyhow!("legacy output closed"))?;
    let native = native.ok_or_else(|| anyhow::anyhow!("native output closed"))?;
    anyhow::ensure!(legacy.results == native.results, "query results differ");
    println!("Both engines produced: {:?}", native.results);
    println!(
        "Native graph revision: {:?}",
        handle.inspector().snapshot().desired.revision
    );

    drasi.stop_computation_graph("native-orders").await?;
    input
        .send_node_insert(
            "two",
            vec!["Order"],
            PropertyMapBuilder::new()
                .with_string("name", "Legacy still running")
                .build(),
        )
        .await?;
    let result = tokio::time::timeout(Duration::from_secs(5), legacy_output.recv())
        .await?
        .ok_or_else(|| anyhow::anyhow!("legacy output stopped"))?;
    println!("After stopping only the native graph: {:?}", result.results);
    // This transient source has no replay: events sent while native is stopped
    // are intentionally not recoverable by that subscriber.
    drasi.shutdown().await?;
    Ok(())
}
