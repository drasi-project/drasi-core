// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![cfg(feature = "otel-context")]
mod telemetry_support;
use drasi_lib::{computation::v1::*, observability::*};
use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::trace::{
    InMemorySpanExporter, Sampler, SdkTracerProvider, SimpleSpanProcessor,
};
use std::{sync::Arc, time::Duration};
use telemetry_support::*;
use tracing_subscriber::prelude::*;

#[tokio::test(flavor = "current_thread")]
async fn real_library_spans_preserve_context_and_share_host_processing() -> anyhow::Result<()> {
    let plugin = load()?;
    clear()?;
    let exporter = InMemorySpanExporter::default();
    let processor = OpenTelemetrySpanProcessor::new(
        SimpleSpanProcessor::new(exporter.clone()),
        Sampler::AlwaysOn,
    );
    let provider = SdkTracerProvider::builder()
        .with_sampler(processor.sampler())
        .with_span_processor(processor.clone())
        .build();
    let collector = NativeSpanCollector::new(Arc::new(processor), Default::default())?;
    let logging = ComponentLogRuntime::new(LogPolicy::new("info")?)?;
    let metrics = RecordingMetrics::default();
    let graph = graph(
        "native-spans",
        &metrics,
        logging.clone(),
        Some(collector.clone()),
        true,
    )?;
    let bound = binding(&graph, "native-spans", 1, logging.clone())?;
    let mut component = create(&plugin, &bound, 5)?;
    component.start().await?;
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("host-test"))),
    );
    let span =
        tracing::dispatcher::with_default(&dispatch, || tracing::info_span!("host.operation"));
    let adapter = OpenTelemetryContextAdapter;
    adapter.prepare_operation(
        &span,
        &ExecutionContext::default(),
        &[],
        RootSampling::Always,
    )?;
    let parent = adapter.span_context(&span);
    let expected_parent = parent.trace().expect("host context").clone();
    run(&mut component, parent).await?;
    drop(span);
    collector.flush(Duration::from_secs(2)).await?;
    provider.force_flush()?;
    let values = exporter.get_finished_spans()?;
    let native = values
        .iter()
        .find(|span| span.name == "plugin.decode")
        .expect("native span exported");
    assert_eq!(native.parent_span_id.to_bytes(), expected_parent.span_id());
    assert_eq!(
        native.span_context.trace_id().to_bytes(),
        expected_parent.trace_id()
    );
    let recorded = observations()?;
    let original: TraceContextV1 = serde_json::from_value(recorded[0]["context"].clone())?;
    assert_eq!(native.span_context.span_id().to_bytes(), original.span_id());
    assert!(native.end_time >= native.start_time);
    assert_eq!(
        values
            .iter()
            .filter(|span| span.name == "host.operation")
            .count(),
        1
    );
    assert_eq!(
        values
            .iter()
            .filter(|span| span.name == "plugin.decode")
            .count(),
        1
    );
    assert!(
        native
            .attributes
            .iter()
            .any(|value| value.key.as_str() == "instance_id"
                && value.value.as_str() == "native-spans")
    );
    assert!(!native.events.events.is_empty());
    let unsampled = ExecutionContext::new(TraceContextV1::new([8; 16], [9; 8], 0, None)?);
    run(&mut component, unsampled).await?;
    collector.flush(Duration::from_secs(2)).await?;
    provider.force_flush()?;
    assert_eq!(
        exporter
            .get_finished_spans()?
            .iter()
            .filter(|span| span.name == "plugin.decode")
            .count(),
        1
    );
    assert_eq!(
        metrics.histogram("drasi.plugin.test-telemetry.decode.duration", &[]),
        vec![0.25, 0.25]
    );
    component.stop().await?;
    drop(component);
    clear()?;
    drop(graph);
    collector.shutdown(Duration::from_secs(2)).await?;
    logging.shutdown(Duration::from_secs(2)).await?;
    provider.shutdown()?;
    Ok(())
}

#[derive(Debug)]
struct BlockedProcessor {
    gate: (std::sync::Mutex<bool>, std::sync::Condvar),
    started: std::sync::Mutex<Option<std::sync::mpsc::SyncSender<()>>>,
    records: std::sync::Mutex<Vec<CollectedSpan>>,
    roots: std::sync::atomic::AtomicUsize,
}

impl BlockedProcessor {
    fn release(&self) {
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
    }
}
impl CompletedSpanProcessor for BlockedProcessor {
    fn process(&self, record: CollectedSpan) -> drasi_lib::observability::Result<()> {
        if let Some(started) = self.started.lock().unwrap().take() {
            started.send(()).unwrap();
        }
        let mut gate = self.gate.0.lock().unwrap();
        while !*gate {
            gate = self.gate.1.wait(gate).unwrap();
        }
        self.records.lock().unwrap().push(record);
        Ok(())
    }
    fn sample_root(
        &self,
        _: &PluginTelemetryConfig,
        _: &RootSpanRequest,
    ) -> drasi_lib::observability::Result<RootSpanDecision> {
        self.roots.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(RootSpanDecision {
            sampled: true,
            trace_state: Some("host=sampled".into()),
        })
    }
}
struct ReleaseProcessor(Arc<BlockedProcessor>);
impl Drop for ReleaseProcessor {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn real_library_span_overflow_never_drops_metrics_logs_or_propagated_context(
) -> anyhow::Result<()> {
    let plugin = load()?;
    clear()?;
    let (sender, started) = std::sync::mpsc::sync_channel(1);
    let processor = Arc::new(BlockedProcessor {
        gate: (std::sync::Mutex::new(false), std::sync::Condvar::new()),
        started: std::sync::Mutex::new(Some(sender)),
        records: Default::default(),
        roots: 0.into(),
    });
    let _release = ReleaseProcessor(processor.clone());
    let collector = NativeSpanCollector::new(
        processor.clone(),
        TelemetryLimits {
            records: 4,
            ..Default::default()
        },
    )?;
    let logging = ComponentLogRuntime::new(LogPolicy::new("info")?)?;
    let metrics = RecordingMetrics::default();
    let graph = graph(
        "saturation",
        &metrics,
        logging.clone(),
        Some(collector.clone()),
        true,
    )?;
    let bound = binding(&graph, "saturation", 1, logging.clone())?;
    let mut component = create(&plugin, &bound, 5)?;
    component.start().await?;
    for _ in 0..8 {
        run(&mut component, Default::default()).await?;
    }
    started.recv_timeout(Duration::from_secs(2))?;
    assert_eq!(collector.retained()?.0, 4);
    assert_eq!(collector.losses().full, 4);
    assert_eq!(bound.diagnostics().snapshot().capture, 0);
    assert_eq!(
        metrics.count("drasi.plugin.test-telemetry.frames_received", &[]),
        5
    );
    assert_eq!(
        metrics.histogram("drasi.plugin.test-telemetry.decode.duration", &[]),
        vec![0.25; 8]
    );
    assert_eq!(processor.roots.load(std::sync::atomic::Ordering::SeqCst), 8);
    let observations = observations()?;
    for value in observations.as_array().expect("native observations") {
        let context: TraceContextV1 = serde_json::from_value(value["context"].clone())?;
        assert!(context.sampled());
        assert_eq!(context.trace_state(), Some("host=sampled"));
    }
    logging.flush().await?;
    let messages = logging
        .registry()
        .get_history_by_key(&ComponentLogKey::new(
            "saturation",
            ComponentLogType::Service,
            "probe",
        ))
        .await;
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.message == "decoded")
            .count(),
        8
    );
    component.stop().await?;
    drop(component);
    assert!(matches!(
        collector.shutdown(Duration::from_millis(10)).await,
        Err(NativeTelemetryError::TimedOut(_))
    ));
    assert_eq!(collector.losses().shutdown_timeout, 3);
    processor.release();
    collector.shutdown(Duration::from_secs(2)).await?;
    assert_eq!(processor.records.lock().unwrap().len(), 1);
    clear()?;
    drop(graph);
    logging.shutdown(Duration::from_secs(2)).await?;
    Ok(())
}
