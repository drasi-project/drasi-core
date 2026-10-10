// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Isolated-process lifecycle qualification: cycles current-thread|multi-thread [replacement].
//! Measures post-disposal live Rust bytes, Tokio tasks and process descriptors.
//! Each graph owns a real temporary file and loopback listening socket.

#![allow(clippy::print_stdout)]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    net::TcpListener,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_lib::computation::v1::*;
use serde::Serialize;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE_OWNERS: AtomicUsize = AtomicUsize::new(0);
static STARTED: AtomicUsize = AtomicUsize::new(0);
static STOPPED: AtomicUsize = AtomicUsize::new(0);
struct MeasuredSystem;

// Preserve System's allocation/layout contract while accounting for live bytes.
unsafe impl GlobalAlloc for MeasuredSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let ptr = unsafe { System.realloc(ptr, layout, size) };
        if !ptr.is_null() {
            if size >= layout.size() {
                LIVE_BYTES.fetch_add(size - layout.size(), Ordering::Relaxed);
            } else {
                LIVE_BYTES.fetch_sub(layout.size() - size, Ordering::Relaxed);
            }
        }
        ptr
    }
}

#[global_allocator]
static ALLOCATOR: MeasuredSystem = MeasuredSystem;

struct Handles {
    _file: tempfile::NamedTempFile,
    _socket: TcpListener,
}

impl Handles {
    fn new(path: &Path) -> Result<Self> {
        let handles = Self {
            _file: tempfile::NamedTempFile::new_in(path)?,
            _socket: TcpListener::bind(("127.0.0.1", 0))?,
        };
        LIVE_OWNERS.fetch_add(1, Ordering::SeqCst);
        Ok(handles)
    }
}

impl Drop for Handles {
    fn drop(&mut self) {
        LIVE_OWNERS.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Service {
    descriptor: ComponentDescriptor,
    handles: Option<Handles>,
}

#[async_trait]
impl ComputationComponent for Service {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        ensure!(self.handles.is_some(), "missing resource owner");
        STARTED.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        if self.handles.take().is_some() {
            STOPPED.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[async_trait]
impl ComputationService for Service {
    async fn run(&mut self) -> Result<()> {
        std::future::pending().await
    }
}

fn descriptors() -> Result<usize> {
    let path = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else if cfg!(target_os = "macos") {
        "/dev/fd"
    } else {
        anyhow::bail!("process descriptor measurement supports Linux and macOS");
    };
    std::fs::read_dir(path)?.try_fold(0, |count, entry| {
        entry?;
        Ok(count + 1)
    })
}

#[derive(Clone, Copy, Serialize)]
struct Sample {
    cycle: usize,
    live_rust_bytes: usize,
    tokio_tasks: usize,
    descriptors: usize,
}

async fn sample(cycle: usize) -> Result<Sample> {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    ensure!(
        LIVE_OWNERS.load(Ordering::SeqCst) == 0,
        "resource owner leaked"
    );
    let descriptors = descriptors()?;
    let tokio_tasks = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    Ok(Sample {
        cycle,
        live_rust_bytes: LIVE_BYTES.load(Ordering::SeqCst),
        tokio_tasks,
        descriptors,
    })
}

async fn cycle(path: &Path, stopped: bool, replacement: bool) -> Result<()> {
    let before = descriptors()?;
    let owners = if replacement { 16 } else { 1 };
    let mut builder = ComputationGraph::builder("resource-soak");
    for index in 0..owners {
        builder = builder.service(Box::new(Service {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new(format!("handles-{index}"))?,
                vec![],
            )?,
            handles: Some(Handles::new(path)?),
        }));
    }
    let mut graph = builder.build()?;
    ensure!(
        descriptors()? >= before + 2 * owners,
        "fixture did not open actual handles"
    );
    let run = graph.run()?;
    let control = run.control();
    let (running, stopped) = tokio::join!(run, async {
        let result = async {
            let started = control
                .start_components(GraphRevision(1), GraphSelection::All)
                .await?;
            ensure!(
                started.summary == OperationSummary::Completed,
                "service did not start"
            );
            if replacement {
                for index in 0..64 {
                    let name = format!("handles-{}", index % owners);
                    let id = ComponentId::try_new(name.as_str())?;
                    let snapshot = control.desired_snapshot();
                    let mut desired = snapshot
                        .select(GraphSelection::Exact(vec![id]))?
                        .components
                        .into_iter()
                        .next()
                        .context("replacement service")?;
                    let descriptor = desired.descriptor.clone();
                    desired.construction = ComponentConstruction::External {
                        binding: name.clone(),
                    };
                    let preview = control
                        .preview(
                            snapshot.revision,
                            vec![DesiredMutation::ReplaceComponent(desired)],
                        )
                        .await?;
                    let mut bindings = TopologyBindings::default();
                    bindings.components.insert(
                        name,
                        ConstructedComponent::service(Box::new(Service {
                            descriptor,
                            handles: Some(Handles::new(path)?),
                        })),
                    );
                    let report = control.reconcile(preview, bindings).await?;
                    ensure!(
                        report.summary == OperationSummary::Completed,
                        "replacement failed: {report:?}"
                    );
                    ensure!(
                        LIVE_OWNERS.load(Ordering::SeqCst) == owners,
                        "replacement retained old handles"
                    );
                    ensure!(
                        descriptors()? == before + 2 * owners,
                        "replacement leaked descriptors"
                    );
                }
            }
            if stopped {
                let report = control
                    .stop_components(control.desired_snapshot().revision, GraphSelection::All)
                    .await?;
                ensure!(
                    report.summary == OperationSummary::Completed,
                    "service did not stop"
                );
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        control.cancel();
        result
    });
    graph.dispose().await?;
    stopped?;
    ensure!(
        matches!(running, Ok(()) | Err(GraphError::Cancelled)),
        "graph failed: {running:?}"
    );
    Ok(())
}

async fn measure(cycles: usize, replacement: bool) -> Result<serde_json::Value> {
    const WARMUP: usize = 64;
    const INTERVAL: usize = 32;
    let directory = tempfile::tempdir()?;
    let mut samples = Vec::with_capacity(cycles / INTERVAL + 2);
    for index in 0..WARMUP {
        tokio::time::timeout(
            Duration::from_secs(10),
            cycle(directory.path(), index % 2 == 0, replacement),
        )
        .await??;
    }
    samples.push(sample(0).await?);
    for index in 1..=cycles {
        tokio::time::timeout(
            Duration::from_secs(10),
            cycle(directory.path(), index % 2 == 0, replacement),
        )
        .await??;
        if index % INTERVAL == 0 || index == cycles {
            samples.push(sample(index).await?);
        }
    }
    let baseline = samples[0];
    ensure!(
        samples
            .iter()
            .all(|s| s.descriptors == baseline.descriptors),
        "process descriptors did not return to baseline"
    );
    ensure!(
        samples
            .iter()
            .all(|s| s.tokio_tasks == baseline.tokio_tasks),
        "Tokio task count did not return to baseline"
    );
    let mean_x = samples.iter().map(|s| s.cycle as f64).sum::<f64>() / samples.len() as f64;
    let mean_y = samples
        .iter()
        .map(|s| s.live_rust_bytes as f64)
        .sum::<f64>()
        / samples.len() as f64;
    let slope = samples
        .iter()
        .map(|s| (s.cycle as f64 - mean_x) * (s.live_rust_bytes as f64 - mean_y))
        .sum::<f64>()
        / samples
            .iter()
            .map(|s| (s.cycle as f64 - mean_x).powi(2))
            .sum::<f64>();
    let last = samples.last().context("samples")?;
    let expected = (cycles + WARMUP) * if replacement { 80 } else { 1 };
    ensure!(
        STARTED.load(Ordering::SeqCst) == expected && STOPPED.load(Ordering::SeqCst) == expected,
        "lifecycle did not start and stop every owner exactly once"
    );
    ensure!(
        last.live_rust_bytes <= baseline.live_rust_bytes + 1024 && slope <= 1.0,
        "live Rust retention grew: baseline={}, final={}, slope={slope} bytes/cycle",
        baseline.live_rust_bytes,
        last.live_rust_bytes
    );
    Ok(serde_json::json!({
        "schema_version": 1, "os": std::env::consts::OS, "architecture": std::env::consts::ARCH,
        "cycles": cycles, "warmup_cycles": WARMUP,
        "live_byte_growth_limit": 1024, "live_byte_slope_limit": 1.0,
        "live_byte_slope": slope, "samples": samples,
        "components_per_graph": if replacement { 16 } else { 1 },
        "replacements_per_graph": if replacement { 64 } else { 0 },
        "verified_file_socket_pairs": expected,
        "verified_starts": STARTED.load(Ordering::SeqCst), "verified_stops": STOPPED.load(Ordering::SeqCst),
    }))
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 2 || args.len() == 3 && args[2] == "replacement",
        "expected cycles current-thread|multi-thread [replacement]"
    );
    let cycles: usize = args[0].parse()?;
    ensure!(
        (1024..=1_000_000).contains(&cycles),
        "cycles must be 1024..1000000"
    );
    let mut runtime = match args[1].as_str() {
        "current-thread" => tokio::runtime::Builder::new_current_thread(),
        "multi-thread" => {
            let mut runtime = tokio::runtime::Builder::new_multi_thread();
            runtime.worker_threads(2);
            runtime
        }
        _ => anyhow::bail!("unknown runtime"),
    };
    let mut report = runtime
        .enable_all()
        .build()?
        .block_on(measure(cycles, args.len() == 3))?;
    report["runtime"] = serde_json::json!(args[1]);
    println!("{report}");
    Ok(())
}
