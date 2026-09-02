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

//! Multi-source sequence isolation (issue #828).
//!
//! When a continuous query joins two sources, the priority queue orders each
//! source's events by that source's own monotonic `sequence`. That only works
//! if every `SourceBase` owns an **independent** sequence counter — not a single
//! process-global one.
//!
//! This test drives two sources with **interleaved** writes (A, B, A, B, A, B)
//! and asserts each source stamps its own `1, 2, 3`. A shared global counter
//! would instead interleave the values (A = `1, 3, 5`, B = `2, 4, 6`), so the
//! equality check below is what proves per-source isolation.

#![allow(clippy::unwrap_used)]

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use drasi_lib::channels::ChangeReceiver;
use drasi_lib::channels::events::SourceEventWrapper;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::Source;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};

/// Streaming subscription settings (no bootstrap, no resume).
fn streaming_settings(source_id: &str, query_id: &str) -> SourceSubscriptionSettings {
    SourceSubscriptionSettings {
        source_id: source_id.to_string(),
        query_id: query_id.to_string(),
        enable_bootstrap: false,
        nodes: HashSet::new(),
        relations: HashSet::new(),
        resume_sequence: None,
        request_position_handle: false,
        resume_from: None,
    }
}

/// Receive the next event and return its framework-stamped sequence.
async fn next_sequence(rx: &mut Box<dyn ChangeReceiver<SourceEventWrapper>>) -> u64 {
    let event = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("timed out waiting for event")
        .expect("event stream closed unexpectedly");
    event
        .sequence
        .expect("every dispatched event must carry a framework sequence")
}

#[tokio::test]
async fn two_joined_sources_maintain_independent_sequences() {
    // Two independent sources, as a cross-source join would subscribe to.
    let (source_a, handle_a) = ApplicationSource::new(
        "orders-source",
        ApplicationSourceConfig {
            properties: HashMap::new(),
            durability: None,
        },
    )
    .unwrap();
    let (source_b, handle_b) = ApplicationSource::new(
        "customers-source",
        ApplicationSourceConfig {
            properties: HashMap::new(),
            durability: None,
        },
    )
    .unwrap();

    // Subscribe directly to each source's change stream so we can observe the
    // raw per-source sequence stamped by the framework.
    let mut rx_a = source_a
        .subscribe(streaming_settings("orders-source", "seq-probe-a"))
        .await
        .unwrap()
        .receiver;
    let mut rx_b = source_b
        .subscribe(streaming_settings("customers-source", "seq-probe-b"))
        .await
        .unwrap()
        .receiver;

    source_a.start().await.unwrap();
    source_b.start().await.unwrap();

    // Interleave writes across the two sources.
    for i in 1..=3 {
        let order_props = PropertyMapBuilder::new()
            .with_string("kind", "order")
            .build();
        handle_a
            .send_node_insert(format!("order-{i}"), vec!["Order"], order_props)
            .await
            .unwrap();

        let customer_props = PropertyMapBuilder::new()
            .with_string("kind", "customer")
            .build();
        handle_b
            .send_node_insert(format!("customer-{i}"), vec!["Customer"], customer_props)
            .await
            .unwrap();
    }

    // Collect the sequences each source stamped on its own stream.
    let mut seqs_a = Vec::new();
    let mut seqs_b = Vec::new();
    for _ in 0..3 {
        seqs_a.push(next_sequence(&mut rx_a).await);
    }
    for _ in 0..3 {
        seqs_b.push(next_sequence(&mut rx_b).await);
    }

    source_a.stop().await.unwrap();
    source_b.stop().await.unwrap();

    // Each source counts from 1 independently.
    assert_eq!(
        seqs_a,
        vec![1, 2, 3],
        "source A must stamp its own monotonic sequence"
    );
    assert_eq!(
        seqs_b,
        vec![1, 2, 3],
        "source B must stamp its own monotonic sequence"
    );

    // The decisive check: both sources produced the SAME sequence values,
    // which is only possible if each has its own counter. A shared global
    // counter would have interleaved them (A = 1,3,5 / B = 2,4,6).
    assert_eq!(
        seqs_a, seqs_b,
        "sources must not share a global sequence counter"
    );
}
