// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    facade::RegisteredMetric, CompletedSpan, PluginMetricHandle, PluginMetricRegistration,
    PluginMetricUpdate, PluginTelemetryConfig, Result, RootSpanDecision, RootSpanRequest,
    ScopedMetricBackend, ScopedMetricEmitter, TelemetryDiagnostics, TelemetryLogRecord,
    TelemetrySubmission,
};
use std::sync::Arc;

/// Local Rust integration only. Native clients translate these calls to the
/// negotiated C callbacks; no trait object or Rust owner crosses that boundary.
pub trait PluginTelemetryBackend: std::fmt::Debug + Send + Sync {
    fn config(&self) -> &PluginTelemetryConfig;
    fn diagnostics(&self) -> &TelemetryDiagnostics;
    fn for_plugin(&self, kind: &str) -> Result<PluginTelemetry>;
    fn register_metric(&self, registration: &PluginMetricRegistration) -> Result<RegisteredMetric>;
    fn register_metric_handle(
        &self,
        registration: &PluginMetricRegistration,
    ) -> Result<Option<PluginMetricHandle>>;
    fn update_metric(&self, update: PluginMetricUpdate) -> Result<()>;
    fn has_span_collector(&self) -> bool;
    fn is_active(&self) -> Result<bool> {
        Err(super::TelemetryError::Unsupported(
            "host-only binding lifetime inspection",
        ))
    }
    fn retire(&self) -> Result<()> {
        Err(super::TelemetryError::Unsupported(
            "host-only binding retirement",
        ))
    }
    fn enabled(&self, _target: &str, _level: super::TelemetryLogLevel, span: bool) -> bool {
        !span || self.config().tracing
    }
    fn submit_encoded_span(&self, bytes: &[u8]) -> TelemetrySubmission;
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
    pub fn register_metric_handle(
        &self,
        registration: &PluginMetricRegistration,
    ) -> Result<Option<PluginMetricHandle>> {
        self.0.register_metric_handle(registration)
    }
    pub fn update_metric(&self, update: PluginMetricUpdate) -> Result<()> {
        self.0.update_metric(update)
    }
    pub fn has_span_collector(&self) -> bool {
        self.0.has_span_collector()
    }
    #[doc(hidden)]
    pub fn is_active(&self) -> Result<bool> {
        self.0.is_active()
    }
    #[doc(hidden)]
    pub fn retire(&self) -> Result<()> {
        self.0.retire()
    }
    pub fn enabled(&self, target: &str, level: super::TelemetryLogLevel, span: bool) -> bool {
        self.0.enabled(target, level, span)
    }
    #[doc(hidden)]
    pub fn binding_key(&self) -> usize {
        self.0.config() as *const PluginTelemetryConfig as usize
    }
    pub fn submit_encoded_span(&self, bytes: &[u8]) -> TelemetrySubmission {
        self.0.submit_encoded_span(bytes)
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

#[derive(Debug, Clone)]
pub struct CollectedSpan {
    pub scope: super::TelemetryScope,
    pub generation: u64,
    pub plugin_kind: String,
    pub span: CompletedSpan,
}

/// Host-owned processing. `process` runs on the collection worker, never in an
/// FFI submission callback. Sampling must be local and must not perform I/O.
pub trait CompletedSpanProcessor: std::fmt::Debug + Send + Sync {
    fn process(&self, span: CollectedSpan) -> Result<()>;
    fn sample_root(
        &self,
        config: &PluginTelemetryConfig,
        request: &RootSpanRequest,
    ) -> Result<RootSpanDecision>;
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
            .or_else(|| super::ExecutionScope::plugin_metrics_disabled().then(Self::disabled))
            .ok_or(super::TelemetryError::Unsupported(
                "unattributed plugin metric registration",
            ))
    }
}
