// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![allow(dead_code)]
use anyhow::{Context, Result};
use drasi_computation_plugin_sdk::{abi, transport, wire};
use drasi_host_sdk::computation::{self, NativeComponentProxy, NativePlugin};
use drasi_lib::{computation::v1::*, observability::*};
use std::{sync::Arc, time::Duration};

mod fixtures;
pub use fixtures::fixture;

#[path = "../../../../lib/tests/computation_support/metrics.rs"]
pub mod metrics;
pub use metrics::RecordingMetrics;

pub fn load() -> Result<Arc<NativePlugin>> {
    let path = fixture("telemetry_fixture");
    anyhow::ensure!(
        path.is_file(),
        "build SDK telemetry_fixture before running this target: {}",
        path.display()
    );
    computation::load(path)
}

pub fn graph(
    instance: &str,
    metrics: &RecordingMetrics,
    logging: ComponentLogRuntime,
    spans: Option<NativeSpanCollector>,
    tracing: bool,
) -> Result<ComputationGraph> {
    let mut options = TelemetryOptions::new(
        TelemetrySettings::new(TelemetryProfile::Basic)
            .with_tracing(tracing)
            .require_native_metrics(),
    )
    .with_instance_id(instance)
    .with_metric_context(MetricRegistrationContext::new(
        Arc::new(metrics.clone()),
        Default::default(),
    )?);
    if let Some(spans) = spans {
        options = options.with_native_span_collector(spans);
    }
    Ok(ComputationGraph::builder("telemetry-graph")
        .with_telemetry(options)
        .with_component_logging(logging)
        .build()?)
}

pub fn binding(
    graph: &ComputationGraph,
    instance: &str,
    generation: u64,
    logging: ComponentLogRuntime,
) -> Result<PluginTelemetry> {
    Ok(graph.telemetry().plugin_binding(
        TelemetryScope::graph(instance, "telemetry-graph")?.component(
            ComponentId::try_new("probe")?,
            ComponentRole::Service,
            ComponentSemanticKind::Service,
        ),
        generation,
        "test-telemetry",
        Some(logging),
    )?)
}

pub fn create(
    plugin: &NativePlugin,
    binding: &PluginTelemetry,
    absolute: u64,
) -> Result<NativeComponentProxy> {
    ExecutionScope::new(Default::default())
        .with_component(binding.config().scope.clone(), binding.config().generation)
        .with_plugin_telemetry(Some(binding.clone()))
        .in_scope(|| {
            plugin.factories()[0].create_component(
                ComponentId::try_new("probe")?,
                serde_json::json!({"absolute": absolute}),
            )
        })
}

pub async fn run(component: &mut NativeComponentProxy, parent: ExecutionContext) -> Result<()> {
    ExecutionScope::new(parent).scope(component.run()).await
}

pub fn update(instance: &str, generation: u64, absolute: bool, value: u64) -> Result<()> {
    let bytes = wire::encode(&(instance, "probe", generation, absolute, value))?;
    unsafe {
        let library = libloading::Library::new(fixture("telemetry_fixture"))?;
        let call = library.get::<unsafe extern "C" fn(abi::BorrowedBytes) -> abi::Status>(
            b"drasi_telemetry_fixture_update\0",
        )?;
        transport::take_status(call(abi::BorrowedBytes::new(&bytes)))?;
    }
    Ok(())
}
pub fn observations() -> Result<serde_json::Value> {
    unsafe {
        let library = libloading::Library::new(fixture("telemetry_fixture"))?;
        let call = library.get::<unsafe extern "C" fn() -> abi::Reply>(
            b"drasi_telemetry_fixture_observations\0",
        )?;
        let bytes = transport::take_reply(call())?;
        wire::decode(&bytes)
    }
}
pub fn clear() -> Result<()> {
    unsafe {
        let library = libloading::Library::new(fixture("telemetry_fixture"))?;
        let call = library
            .get::<unsafe extern "C" fn() -> abi::Status>(b"drasi_telemetry_fixture_clear\0")?;
        transport::take_status(call())?;
    }
    Ok(())
}

pub async fn metrics_contract() -> Result<()> {
    let plugin = load()?;
    clear()?;
    let metrics = RecordingMetrics::default();
    let logging = ComponentLogRuntime::new(LogPolicy::new("info")?)?;
    let graph = graph("native-metrics", &metrics, logging.clone(), None, false)?;
    let old = binding(&graph, "native-metrics", 1, logging.clone())?;
    let mut a = create(&plugin, &old, 100)?;
    a.start().await?;
    run(&mut a, Default::default()).await?;
    assert_eq!(
        metrics.count("drasi.plugin.test-telemetry.frames_received", &[]),
        100
    );
    a.stop().await?;
    let new = binding(&graph, "native-metrics", 2, logging.clone())?;
    let mut b = create(&plugin, &new, 10)?;
    b.start().await?;
    run(&mut b, Default::default()).await?;
    update("native-metrics", 2, true, 10)?;
    update("native-metrics", 2, true, 8)?;
    update("native-metrics", 2, false, 5)?;
    update("native-metrics", 2, true, 15)?;
    update("native-metrics", 2, true, 20)?;
    update("native-metrics", 1, true, 120)?;
    assert_eq!(
        metrics.count("drasi.plugin.test-telemetry.frames_received", &[]),
        120
    );
    assert_eq!(old.diagnostics().snapshot().metric, 1);
    assert_eq!(
        metrics.histogram("drasi.plugin.test-telemetry.decode.duration", &[]),
        vec![0.25, 0.25]
    );
    assert!(metrics
        .gauge_samples("drasi.plugin.test-telemetry.queued_frames", &[])
        .windows(2)
        .any(|values| values == [7.0, 0.0]));
    std::thread::scope(|threads| {
        let workers: Vec<_> = (0..4)
            .map(|_| {
                threads.spawn(|| -> Result<()> {
                    for _ in 0..25 {
                        update("native-metrics", 2, false, 1)?;
                    }
                    Ok(())
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("native update worker")?;
        }
        Ok::<_, anyhow::Error>(())
    })?;
    assert_eq!(
        metrics.count("drasi.plugin.test-telemetry.frames_received", &[]),
        220
    );
    logging.flush().await?;
    let key = ComponentLogKey::new("native-metrics", ComponentLogType::Service, "probe");
    assert!(logging.registry().log_count_by_key(&key).await >= 7);
    let values = observations()?;
    assert_eq!(values.as_array().context("observations")?.len(), 2);
    b.stop().await?;
    drop(a);
    drop(b);
    assert_eq!(
        metrics
            .gauge_samples("drasi.plugin.test-telemetry.queued_frames", &[])
            .last(),
        Some(&0.0)
    );
    update("native-metrics", 2, false, 1)?;
    assert_eq!(
        metrics.count("drasi.plugin.test-telemetry.frames_received", &[]),
        220
    );
    clear()?;
    let renewed = binding(&graph, "native-metrics", 2, logging.clone())?;
    let mut restarted = create(&plugin, &renewed, 3)?;
    restarted.start().await?;
    run(&mut restarted, Default::default()).await?;
    assert_eq!(
        metrics.count("drasi.plugin.test-telemetry.frames_received", &[]),
        223
    );
    restarted.stop().await?;
    drop(restarted);
    clear()?;
    drop(graph);
    logging.shutdown(Duration::from_secs(2)).await?;
    Ok(())
}
