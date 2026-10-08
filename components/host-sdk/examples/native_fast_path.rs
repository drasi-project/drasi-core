// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Recovery-disabled native arithmetic through the actual graph and cdylib ABI.
//! Set DRASI_NATIVE_STANDARD_PLUGIN to a separately built release standard plugin.
//! Arguments: <events> <in-flight window>. Allocation counters cover the host,
//! not the cdylib's separate System allocator; process CPU/RSS include both.

#![allow(clippy::print_stdout)]

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_host_sdk::computation;
use drasi_lib::{computation::v1::*, DrasiLib};
use serde_json::json;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

struct MeasuredSystem;
static MEASURING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

fn record(size: usize) {
    if MEASURING.load(Ordering::Relaxed) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(size as u64, Ordering::Relaxed);
    }
}

// The instrumentation preserves System's allocation/deallocation contract.
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
        unsafe {
            System.dealloc(ptr, layout);
        }
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

struct Input {
    descriptor: ComponentDescriptor,
    values: mpsc::Receiver<u64>,
}
#[async_trait]
impl ComputationComponent for Input {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for Input {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        let Some(value) = self.values.recv().await else {
            return Ok(None);
        };
        let envelope = GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("input", "one"),
                        labels: vec!["Bench".into()].into(),
                        effective_from: value + 1,
                    },
                    properties: ElementPropertyMap::from(json!({"value": value})),
                },
            },
            StreamId::try_new("input/out")?,
            value + 1,
            None,
        )?;
        Ok(Some(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope,
        }))
    }
}

struct Output {
    descriptor: ComponentDescriptor,
    values: mpsc::Sender<u64>,
}
#[async_trait]
impl ComputationComponent for Output {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSink for Output {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        let changes = GraphChangeCodec::decode_changes(&input.envelope)?;
        let [SourceChange::Insert {
            element: Element::Node { properties, .. },
        }] = changes.as_slice()
        else {
            anyhow::bail!("unexpected native output");
        };
        let Some(ElementValue::Integer(value)) = properties.get("value") else {
            anyhow::bail!("native output has no integer value");
        };
        self.values.send(u64::try_from(*value)?).await?;
        Ok(())
    }
}

fn id(value: &str) -> Result<ComponentId> {
    Ok(ComponentId::try_new(value)?)
}
fn endpoint(component: &str, port: &str) -> Result<Endpoint> {
    Ok(Endpoint::new(id(component)?, PortId::try_new(port)?))
}
fn descriptor(name: &str, port: &str, direction: PortDirection) -> Result<ComponentDescriptor> {
    Ok(ComponentDescriptor::try_new(
        id(name)?,
        vec![PortDescriptor::new(
            PortId::try_new(port)?,
            direction,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )?)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let events: usize = args.next().context("expected event count")?.parse()?;
    let window: usize = args.next().context("expected in-flight window")?.parse()?;
    ensure!(
        (1..=10_000_000).contains(&events) && (1..=64).contains(&window) && args.next().is_none(),
        "expected events 1..10000000 and window 1..64"
    );
    let plugin = computation::load(
        std::env::var("DRASI_NATIVE_STANDARD_PLUGIN")
            .context("set DRASI_NATIVE_STANDARD_PLUGIN to the release cdylib")?,
    )?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.standard/arithmetic"
        })
        .context("missing native arithmetic factory")?
        .clone();
    let (send, values) = mpsc::channel(64);
    let (output, mut receive) = mpsc::channel(64);
    let batch = ComputationGraph::builder("__drasi_lib_runtime__")
        .source(Box::new(Input {
            descriptor: descriptor("input", "out", PortDirection::Output)?,
            values,
        }))
        .component(
            factory.specification(id("native")?, json!({"stream":"native/out","add":1}))?,
            factory,
        )
        .sink(Box::new(Output {
            descriptor: descriptor("output", "in", PortDirection::Input)?,
            values: output,
        }))
        .bind_stream(endpoint("input", "out")?, StreamId::try_new("input/out")?)
        .bind_stream(endpoint("native", "out")?, StreamId::try_new("native/out")?)
        .connect(
            EdgeDefinition::new(endpoint("input", "out")?, endpoint("native", "in")?),
            Box::new(BoundedPipeConfig { capacity: 64 }),
        )
        .connect(
            EdgeDefinition::new(endpoint("native", "out")?, endpoint("output", "in")?),
            Box::new(BoundedPipeConfig { capacity: 64 }),
        )
        .build_components()?;
    let core = DrasiLib::builder()
        .with_id("native-fast-path")
        .build()
        .await?;
    core.add_components(batch).await?;
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
                send.send(sequence).await?;
                sequence += 1;
            }
            for (offset, sent) in starts.iter().enumerate() {
                let value = tokio::time::timeout(Duration::from_secs(10), receive.recv())
                    .await?
                    .context("native output closed")?;
                ensure!(value == first + offset as u64 + 1, "native output mismatch");
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
    println!(
        "{}",
        json!({
            "events": events, "window": window, "seconds": seconds,
            "events_per_second": events as f64 / seconds,
            "latency_p50_ns": latencies[events / 2],
            "latency_p99_ns": latencies[(events * 99 / 100).min(events - 1)],
            "allocations": allocations, "allocated_bytes": allocated_bytes,
            "allocation_scope": "host-only; plugin uses its separate System allocator",
        })
    );
    Ok(())
}
