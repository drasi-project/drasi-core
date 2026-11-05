// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    facade::RegisteredMetric, CompletedSpan, PluginMetricRegistration, PluginTelemetryConfig,
    Result, RootSpanDecision, RootSpanRequest, ScopedMetricBackend, ScopedMetricEmitter,
    TelemetryDiagnostics, TelemetryError, TelemetryLogRecord, TelemetrySubmission,
};
use std::sync::Arc;

/// Local Rust integration only. Native clients translate these calls to the
/// negotiated C callbacks; no trait object or Rust owner crosses that boundary.
pub trait PluginTelemetryBackend: std::fmt::Debug + Send + Sync {
    fn config(&self) -> &PluginTelemetryConfig;
    fn diagnostics(&self) -> &TelemetryDiagnostics;
    fn for_plugin(&self, kind: &str) -> Result<PluginTelemetry>;
    fn register_metric(&self, registration: &PluginMetricRegistration) -> Result<RegisteredMetric>;
    fn submit_span(&self, span: &CompletedSpan) -> TelemetrySubmission;
    fn submit_log(&self, record: &TelemetryLogRecord) -> TelemetrySubmission;
    fn sample_root(&self, request: &RootSpanRequest) -> Result<RootSpanDecision>;
}

#[derive(Debug, Clone)]
pub struct PluginTelemetry(Arc<dyn PluginTelemetryBackend>);

impl PluginTelemetry {
    pub fn new(backend: Arc<dyn PluginTelemetryBackend>) -> Self {
        Self(backend)
    }
    pub fn config(&self) -> &PluginTelemetryConfig {
        self.0.config()
    }
    pub fn diagnostics(&self) -> &TelemetryDiagnostics {
        self.0.diagnostics()
    }
    pub fn for_plugin(&self, kind: &str) -> Result<Self> {
        self.0.for_plugin(kind)
    }
    pub fn metrics(&self) -> ScopedMetricEmitter {
        ScopedMetricEmitter::new(Arc::new(self.clone()))
    }
    pub fn submit_span(&self, span: &CompletedSpan) -> TelemetrySubmission {
        self.0.submit_span(span)
    }
    pub fn submit_log(&self, record: &TelemetryLogRecord) -> TelemetrySubmission {
        self.0.submit_log(record)
    }
    pub fn sample_root(&self, request: &RootSpanRequest) -> Result<RootSpanDecision> {
        self.0.sample_root(request)
    }
}

impl ScopedMetricBackend for PluginTelemetry {
    fn register(&self, registration: &PluginMetricRegistration) -> Result<RegisteredMetric> {
        self.0.register_metric(registration)
    }
}

#[cfg(feature = "tracing-context")]
impl ScopedMetricEmitter {
    pub fn current() -> Result<Self> {
        super::ExecutionScope::current_plugin_telemetry()
            .map(|binding| binding.metrics())
            .ok_or(TelemetryError::Unsupported(
                "unattributed plugin metric registration",
            ))
    }
}
