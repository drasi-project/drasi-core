// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Steady-state, recovery-disabled measurement through the ordinary graph API.
//! Run a release build with arguments: <events> <in-flight window>.
//! Startup, warmup and shutdown are excluded from allocation/latency counters.
//! The allocator delegates to System; no alternate allocator or plugins are loaded.

#![allow(clippy::print_stdout)]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        OnceLock,
    },
    time::{Duration, Instant},
};

use anyhow::{ensure, Context, Result};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_lib::{channels::ResultDiff, DrasiLib, Query};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig};

struct MeasuredSystem;
static MEASURING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static ALLOCATION_SIZES: OnceLock<Vec<AtomicU64>> = OnceLock::new();

fn record(size: usize) {
    if MEASURING.load(Ordering::Relaxed) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(size as u64, Ordering::Relaxed);
        if let Some(sizes) = ALLOCATION_SIZES.get() {
            sizes[size.min(sizes.len() - 1)].fetch_add(1, Ordering::Relaxed);
        }
    }
}

// Every allocation and deallocation retains System's original layout contract.
unsafe impl GlobalAlloc for MeasuredSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            record(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let ptr = unsafe { System.realloc(ptr, layout, size) };
        if !ptr.is_null() {
            record(size);
        }
        ptr
    }
}

#[global_allocator]
static ALLOCATOR: MeasuredSystem = MeasuredSystem;

fn change(value: u64) -> SourceChange {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("input", "one"),
            labels: vec!["Bench".into()].into(),
            effective_from: value + 1,
        },
        properties: ElementPropertyMap::from(BTreeMap::from([(
            "value".into(),
            ElementValue::Integer(value as i64),
        )])),
    };
    if value == 0 {
        SourceChange::Insert { element }
    } else {
        SourceChange::Update { element }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    if std::env::var_os("DRASI_FAST_PATH_ALLOCATION_SIZES").is_some() {
        let _ = ALLOCATION_SIZES.set((0..=65_536).map(|_| AtomicU64::new(0)).collect());
    }
    let mut args = std::env::args().skip(1);
    let events: usize = args.next().context("expected event count")?.parse()?;
    let window: usize = args.next().context("expected in-flight window")?.parse()?;
    ensure!(
        (1..=10_000_000).contains(&events) && (1..=64).contains(&window) && args.next().is_none(),
        "expected event count 1..10000000 and window 1..64"
    );
    let (source, input) = ApplicationSource::new(
        "input",
        ApplicationSourceConfig {
            properties: Default::default(),
            durability: None,
        },
    )?;
    let (reaction, output) = ApplicationReaction::new("output", vec!["projection".into()]);
    let mut receiver = output.take_receiver().await.context("output receiver")?;
    let core = DrasiLib::builder()
        .with_id("fast-path")
        .with_source(source)
        .with_query(
            Query::cypher("projection")
                .query("MATCH (n:Bench) RETURN n.value AS value")
                .from_source("input")
                .enable_bootstrap(false)
                .build(),
        )
        .with_reaction(reaction)
        .build()
        .await?;
    core.start().await?;
    let mut sequence = 0_u64;
    let mut starts = Vec::with_capacity(window);
    let mut latencies = Vec::with_capacity(events);
    let mut seconds = 0.0;
    for (count, measured) in [(2_048, false), (events, true)] {
        let start = Instant::now();
        if measured {
            ALLOCATIONS.store(0, Ordering::Relaxed);
            ALLOCATED_BYTES.store(0, Ordering::Relaxed);
            MEASURING.store(true, Ordering::Relaxed);
        }
        let mut processed = 0;
        while processed < count {
            let batch = window.min(count - processed);
            let first = sequence;
            starts.clear();
            for _ in 0..batch {
                starts.push(Instant::now());
                input.send(change(sequence)).await?;
                sequence += 1;
            }
            for (offset, sent) in starts.iter().enumerate() {
                let result = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
                    .await?
                    .context("output closed")?;
                let row = match result.results.as_slice() {
                    [ResultDiff::Add { data, .. }] => data,
                    [ResultDiff::Update { after, .. }] => after,
                    _ => anyhow::bail!("expected one projection result: {:?}", result.results),
                };
                ensure!(
                    row["value"].as_u64() == Some(first + offset as u64),
                    "output mismatch"
                );
                if measured {
                    latencies.push(sent.elapsed().as_nanos() as u64);
                }
            }
            processed += batch;
        }
        if measured {
            seconds = start.elapsed().as_secs_f64();
            MEASURING.store(false, Ordering::Relaxed);
        }
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let allocated_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    core.shutdown().await?;
    latencies.sort_unstable();
    let allocation_sizes: BTreeMap<_, _> = ALLOCATION_SIZES
        .get()
        .into_iter()
        .flat_map(|sizes| sizes.iter().enumerate())
        .filter_map(|(size, count)| {
            let count = count.load(Ordering::Relaxed);
            (count > 0).then_some((size, count))
        })
        .collect();
    println!(
        "{}",
        serde_json::json!({
            "events": events,
            "window": window,
            "seconds": seconds,
            "events_per_second": events as f64 / seconds,
            "latency_p50_ns": latencies[(events - 1) / 2],
            "latency_p99_ns": latencies[(events - 1) * 99 / 100],
            "allocations": allocations,
            "allocated_bytes": allocated_bytes,
            "allocation_sizes": allocation_sizes,
        })
    );
    Ok(())
}
