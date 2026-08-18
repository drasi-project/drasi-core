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

use anyhow::Result;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_log::LogReaction;
use drasi_source_otel::OtelSource;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let bind = std::env::var("OTEL_GRPC_BIND").unwrap_or_else(|_| "127.0.0.1:4317".to_string());
    let source = OtelSource::builder("otel")
        .with_grpc_bind(&bind)
        .with_metric_allowlist(["latency_p99_ms"])
        .with_heartbeat_metric("health.heartbeat")
        .with_dependency_ttl_secs(60)
        .build()?;

    let query = Query::cypher("checkout-latency")
        .query(
            "MATCH (s:Service)-[:REPORTS]->(m:Metric) \
             RETURN s.name AS service, m.value AS latencyMs",
        )
        .from_source("otel")
        .auto_start(true)
        .build();

    let reaction = LogReaction::builder("log")
        .with_query("checkout-latency")
        .build()?;

    let drasi = DrasiLib::builder()
        .with_id("otel-getting-started")
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await?;

    drasi.start().await?;
    log::info!("Listening for OTLP on {bind}. Send a gauge with: cargo run --bin send-otlp");
    tokio::signal::ctrl_c().await?;
    drasi.stop().await?;
    Ok(())
}
