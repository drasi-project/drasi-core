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

//! Independent native query pipelines sharing one source in a DrasiLib instance.
//! Run: cargo run -p drasi-lib --example computation_instance

use anyhow::{Context, Result};
use drasi_lib::{computation::v1::*, error::OperationFailures, DrasiLib, Query};
use drasi_source_application::{NativeApplicationSource, PropertyMapBuilder};
use futures::StreamExt;
use std::{num::NonZeroUsize, time::Duration};

const INSTANCE: &str = "parallel-example";

#[tokio::main]
async fn main() -> Result<()> {
    let drasi = DrasiLib::builder().with_id(INSTANCE).build().await?;
    let result = run(&drasi).await;
    let cleanup = drasi.shutdown().await;
    let failures: Vec<_> = result
        .err()
        .into_iter()
        .chain(cleanup.err().map(Into::into))
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(OperationFailures::new("native pipeline example failed", failures).into())
    }
}

#[allow(clippy::print_stdout)]
async fn run(drasi: &DrasiLib) -> Result<()> {
    let stream = StreamId::try_new("orders/out")?;
    let (source, input) = NativeApplicationSource::channel(
        INSTANCE,
        drasi
            .computation_control()?
            .desired_snapshot()
            .id
            .to_string(),
        "orders",
        stream.clone(),
        NonZeroUsize::new(16).context("source capacity")?,
    )?;
    let output = source.output();
    let added = drasi
        .add_components(
            ComponentBatch::builder()
                .source(Box::new(source))
                .bind_stream(output.clone(), stream)
                .build()?,
        )
        .await?;
    anyhow::ensure!(added.committed, "source declaration failed: {added:?}");
    let query = |id| {
        Query::cypher(id)
            .query("MATCH (o:Order) RETURN o.name AS name")
            .from_source("orders")
            .enable_bootstrap(false)
            .build()
    };
    let pipeline = drasi
        .computation_pipeline()?
        .native_source(output)?
        .query(query("primary"))
        .query(query("observer"))
        .build()?;
    let added = drasi.add_components(pipeline).await?;
    anyhow::ensure!(added.committed, "query declarations failed: {added:?}");
    drasi.start().await?;
    let mut primary = drasi.subscribe_query_results("primary").await?.stream;
    let mut observer = drasi.subscribe_query_results("observer").await?.stream;
    input
        .send_node_insert(
            "one",
            vec!["Order"],
            PropertyMapBuilder::new()
                .with_string("name", "First order")
                .build(),
        )
        .await?;
    let (first, second) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(primary.next(), observer.next())
    })
    .await?;
    let first = first.context("primary reader closed")??;
    let second = second.context("observer reader closed")??;
    anyhow::ensure!(first.results == second.results, "query results differ");
    println!("Both native queries produced: {:?}", second.results);
    println!(
        "Instance computation revision: {:?}",
        drasi.computation_info().await?.revision
    );

    drasi.stop_query("observer").await?;
    input
        .send_node_insert(
            "two",
            vec!["Order"],
            PropertyMapBuilder::new()
                .with_string("name", "Primary pipeline still running")
                .build(),
        )
        .await?;
    let result = tokio::time::timeout(Duration::from_secs(5), primary.next())
        .await?
        .context("primary reader stopped")??;
    println!("With the observer query stopped: {:?}", result.results);
    drasi.start_query("observer").await?;
    let resumed = tokio::time::timeout(Duration::from_secs(5), observer.next())
        .await?
        .context("observer reader stopped")??;
    anyhow::ensure!(
        result.results == resumed.results,
        "buffered update was not preserved"
    );
    println!("After restarting the observer query: {:?}", resumed.results);
    Ok(())
}
