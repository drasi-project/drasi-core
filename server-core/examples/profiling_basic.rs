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

//! Basic Profiling Example
//!
//! This example demonstrates how to enable and use profiling to track
//! end-to-end latency through the Drasi pipeline:
//! Source → Query → Reaction
//!
//! Run with: cargo run --example profiling_basic

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_server_core::{
    channels::{QueryResult, SourceEvent, SourceEventWrapper},
    profiling::{timestamp_ns, ProfilingMetadata},
};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    println!("=== Basic Profiling Example ===\n");

    // STEP 1: Create profiling metadata at the source
    println!("Step 1: Creating source event with profiling");
    let mut profiling = ProfilingMetadata::new();

    // Capture the timestamp when the source sends the event
    profiling.source_send_ns = Some(timestamp_ns());
    println!("  source_send_ns: {:?}", profiling.source_send_ns);

    // Create a sample node element
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("example_source", "user123"),
            labels: Arc::from(vec![Arc::from("User")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::new(),
    };

    let source_change = SourceChange::Insert { element };

    // Wrap the source event with profiling metadata
    let event_wrapper = SourceEventWrapper::with_profiling(
        "example_source".to_string(),
        SourceEvent::Change(source_change),
        chrono::Utc::now(),
        profiling.clone(),
    );

    println!("  Created SourceEventWrapper with profiling\n");

    // STEP 2: Simulate query receiving and processing
    println!("Step 2: Query receives and processes the event");

    // Small delay to simulate network/channel latency
    tokio::time::sleep(tokio::time::Duration::from_micros(100)).await;

    let mut query_profiling = event_wrapper.profiling.unwrap();
    query_profiling.query_receive_ns = Some(timestamp_ns());
    println!("  query_receive_ns: {:?}", query_profiling.query_receive_ns);

    // Simulate query core processing
    tokio::time::sleep(tokio::time::Duration::from_micros(50)).await;
    query_profiling.query_core_call_ns = Some(timestamp_ns());
    println!(
        "  query_core_call_ns: {:?}",
        query_profiling.query_core_call_ns
    );

    tokio::time::sleep(tokio::time::Duration::from_micros(200)).await;
    query_profiling.query_core_return_ns = Some(timestamp_ns());
    println!(
        "  query_core_return_ns: {:?}",
        query_profiling.query_core_return_ns
    );

    query_profiling.query_send_ns = Some(timestamp_ns());
    println!("  query_send_ns: {:?}\n", query_profiling.query_send_ns);

    // Create query result with profiling
    let _query_result = QueryResult::with_profiling(
        "example_query".to_string(),
        chrono::Utc::now(),
        vec![serde_json::json!({
            "type": "add",
            "data": {"id": "user123", "name": "John Doe"}
        })],
        std::collections::HashMap::new(),
        query_profiling.clone(),
    );

    println!("  Created QueryResult with profiling\n");

    // STEP 3: Simulate reaction receiving and processing
    println!("Step 3: Reaction receives and processes the result");

    tokio::time::sleep(tokio::time::Duration::from_micros(50)).await;

    let mut reaction_profiling = query_profiling;
    reaction_profiling.reaction_receive_ns = Some(timestamp_ns());
    println!(
        "  reaction_receive_ns: {:?}",
        reaction_profiling.reaction_receive_ns
    );

    // Simulate reaction processing (e.g., HTTP call, database write)
    tokio::time::sleep(tokio::time::Duration::from_micros(300)).await;

    reaction_profiling.reaction_complete_ns = Some(timestamp_ns());
    println!(
        "  reaction_complete_ns: {:?}\n",
        reaction_profiling.reaction_complete_ns
    );

    // STEP 4: Calculate and display latencies
    println!("Step 4: Calculating end-to-end latencies");

    if let Some(source_send) = reaction_profiling.source_send_ns {
        if let Some(query_recv) = reaction_profiling.query_receive_ns {
            let source_to_query = query_recv - source_send;
            println!(
                "  Source → Query:       {} ns ({:.3} ms)",
                source_to_query,
                source_to_query as f64 / 1_000_000.0
            );
        }

        if let Some(query_send) = reaction_profiling.query_send_ns {
            if let Some(reaction_recv) = reaction_profiling.reaction_receive_ns {
                let query_to_reaction = reaction_recv - query_send;
                println!(
                    "  Query → Reaction:     {} ns ({:.3} ms)",
                    query_to_reaction,
                    query_to_reaction as f64 / 1_000_000.0
                );
            }
        }

        if let Some(query_core_call) = reaction_profiling.query_core_call_ns {
            if let Some(query_core_return) = reaction_profiling.query_core_return_ns {
                let query_core_time = query_core_return - query_core_call;
                println!(
                    "  Query Core Time:      {} ns ({:.3} ms)",
                    query_core_time,
                    query_core_time as f64 / 1_000_000.0
                );
            }
        }

        if let Some(reaction_recv) = reaction_profiling.reaction_receive_ns {
            if let Some(reaction_complete) = reaction_profiling.reaction_complete_ns {
                let reaction_time = reaction_complete - reaction_recv;
                println!(
                    "  Reaction Time:        {} ns ({:.3} ms)",
                    reaction_time,
                    reaction_time as f64 / 1_000_000.0
                );
            }
        }

        if let Some(reaction_complete) = reaction_profiling.reaction_complete_ns {
            let total_latency = reaction_complete - source_send;
            println!(
                "\n  Total End-to-End:     {} ns ({:.3} ms)",
                total_latency,
                total_latency as f64 / 1_000_000.0
            );
        }
    }

    println!("\n=== Profiling Complete ===");
    println!("\nKey Takeaways:");
    println!("1. Create ProfilingMetadata at the source and set source_send_ns");
    println!("2. Pass profiling through SourceEventWrapper, QueryResult");
    println!("3. Capture timestamps at each pipeline stage");
    println!("4. Calculate latencies by subtracting timestamps");
    println!("\nFor production use, consider the ProfilerReaction which");
    println!("automatically collects statistics across many events.");
}
