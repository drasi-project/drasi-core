// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_plugin_sdk::telemetry::*;
use metrics::{Counter, Histogram};
use std::{sync::Arc, time::Instant};

struct FrameMetrics {
    received: Counter,
    decode: Histogram,
}

impl FrameMetrics {
    fn new(emitter: &ScopedMetricEmitter) -> Result<Self> {
        Ok(Self {
            received: emitter.counter("frames_received", MetricUnit::One, "Upstream frames")?,
            decode: emitter.histogram("decode.duration", MetricUnit::Seconds, "Frame decoding")?,
        })
    }

    fn decode(&self, frame: &[u8]) -> serde_json::Result<serde_json::Value> {
        self.received.increment(1);
        let started = Instant::now();
        let result = serde_json::from_slice(frame);
        self.decode.record(started.elapsed().as_secs_f64());
        result
    }
}

fn main() -> anyhow::Result<()> {
    use drasi_computation_contracts::{ComponentId, ComponentRole, ComponentSemanticKind};
    // The host chooses a recorder; replace NoopRecorder with its export adapter.
    let recorder =
        MetricRegistrationContext::new(Arc::new(metrics::NoopRecorder), Default::default())?;
    let binding = LocalMetricBinding::new(
        Some(recorder),
        Arc::new(PluginTelemetryConfig {
            scope: TelemetryScope::graph("example", "graph")?.component(
                ComponentId::try_new("feed")?,
                ComponentRole::Source,
                ComponentSemanticKind::Source,
            ),
            generation: 1,
            plugin_kind: "changefeed".into(),
            tracing: false,
            metrics: true,
            native_metrics_required: false,
            native_query_observations_required: false,
            trace_directives: "off".into(),
            log_directives: "off".into(),
            metric_exclusions: vec![],
            sampling: PluginRootSampling::Never,
            limits: Default::default(),
        }),
        Default::default(),
        Default::default(),
    )?;
    // Inside a native factory, obtain this emitter with ScopedMetricEmitter::current().
    let measurements = FrameMetrics::new(&binding.emitter())?;
    assert_eq!(measurements.decode(br#"{"id":1}"#)?["id"], 1);
    assert!(measurements.decode(b"invalid JSON").is_err());
    Ok(())
}
