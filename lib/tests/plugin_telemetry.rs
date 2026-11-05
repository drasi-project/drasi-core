// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[path = "computation_support/metrics.rs"]
mod metrics;

use drasi_lib::{computation::v1::*, observability::*};
use metrics::RecordingMetrics;
use std::{sync::Arc, time::Duration};

fn graph(recorder: &RecordingMetrics, instance: &str) -> anyhow::Result<ComputationGraph> {
    Ok(ComputationGraph::builder("plugin-graph")
        .with_telemetry(
            TelemetryOptions::new(
                TelemetrySettings::new(TelemetryProfile::Basic).with_tracing(false),
            )
            .with_instance_id(instance)
            .with_metric_context(MetricRegistrationContext::new(
                Arc::new(recorder.clone()),
                Default::default(),
            )?),
        )
        .build()?)
}

fn binding(
    graph: &ComputationGraph,
    instance: &str,
    generation: u64,
) -> anyhow::Result<PluginTelemetry> {
    Ok(graph.telemetry().plugin_binding(
        TelemetryScope::graph(instance, "plugin-graph")?.component(
            ComponentId::try_new("probe")?,
            ComponentRole::Service,
            ComponentSemanticKind::Service,
        ),
        generation,
        "test-telemetry",
        None,
    )?)
}

#[test]
fn direct_static_handles_match_native_replacement_accounting_and_isolate_instances(
) -> anyhow::Result<()> {
    let recorder = RecordingMetrics::default();
    let graph_a = graph(&recorder, "one")?;
    let graph_b = graph(&recorder, "two")?;
    let old = binding(&graph_a, "one", 1)?;
    let emitter = old.metrics();
    let counter = emitter.counter("frames_received", MetricUnit::One, "Frames")?;
    let gauge = emitter.gauge("queued_frames", MetricUnit::One, "Queued")?;
    counter.absolute(100);
    gauge.set(7.0);
    let next = binding(&graph_a, "one", 2)?;
    let replacement = next
        .metrics()
        .counter("frames_received", MetricUnit::One, "Frames")?;
    for absolute in [10, 10, 8] {
        replacement.absolute(absolute);
    }
    replacement.clone().increment(5);
    let again = next
        .metrics()
        .counter("frames_received", MetricUnit::One, "Frames")?;
    again.absolute(15);
    again.absolute(20);
    counter.absolute(120);
    gauge.set(10.0);
    assert_eq!(
        recorder.count(
            "drasi.plugin.test-telemetry.frames_received",
            &[("instance_id", "one")]
        ),
        120
    );
    assert_eq!(
        recorder.gauge_samples("drasi.plugin.test-telemetry.queued_frames", &[]),
        vec![0.0, 7.0, 0.0]
    );
    let other = binding(&graph_b, "two", 1)?;
    other
        .metrics()
        .counter("frames_received", MetricUnit::One, "Frames")?
        .absolute(4);
    assert_eq!(
        recorder.count(
            "drasi.plugin.test-telemetry.frames_received",
            &[("instance_id", "two")]
        ),
        4
    );
    drop(graph_a);
    replacement.increment(10);
    assert_eq!(
        recorder.count(
            "drasi.plugin.test-telemetry.frames_received",
            &[("instance_id", "one")]
        ),
        120
    );
    other
        .metrics()
        .counter("frames_received", MetricUnit::One, "Frames")?
        .increment(1);
    assert_eq!(
        recorder.count(
            "drasi.plugin.test-telemetry.frames_received",
            &[("instance_id", "two")]
        ),
        5
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_log_metadata_preserves_original_time_target_fields_and_scope() -> anyhow::Result<()>
{
    let recorder = RecordingMetrics::default();
    let graph = graph(&recorder, "logs")?;
    let logging = ComponentLogRuntime::new(LogPolicy::new("info")?)?;
    let binding = graph.telemetry().plugin_binding(
        TelemetryScope::graph("logs", "plugin-graph")?.component(
            ComponentId::try_new("probe")?,
            ComponentRole::Service,
            ComponentSemanticKind::Service,
        ),
        1,
        "test",
        Some(logging.clone()),
    )?;
    let context = TraceContextV1::new([1; 16], [2; 8], 0, None)?;
    let record = TelemetryLogRecord {
        time_unix_ns: 1_234_567_890,
        level: TelemetryLogLevel::Info,
        target: "test::decode".into(),
        message: "decoded".into(),
        attributes: vec![("count".into(), TelemetryAttribute::I64(3))],
        context: Some(context.clone()),
    };
    assert_eq!(binding.submit_log(&record), TelemetrySubmission::Accepted);
    logging.flush().await?;
    let messages = logging
        .registry()
        .get_history_by_key(&ComponentLogKey::new(
            "logs",
            ComponentLogType::Service,
            "probe",
        ))
        .await;
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].timestamp.timestamp_nanos_opt(),
        Some(1_234_567_890)
    );
    let metadata = messages[0].metadata.as_ref().expect("native log metadata");
    assert_eq!(metadata.target, record.target);
    assert_eq!(metadata.context, Some(context));
    assert_eq!(metadata.attributes, record.attributes);
    assert_eq!(metadata.graph_id, "plugin-graph");
    logging.shutdown(Duration::from_secs(2)).await?;
    Ok(())
}
