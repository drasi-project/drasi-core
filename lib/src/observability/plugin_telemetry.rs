// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::sync::{Arc, Weak};

pub(super) fn bind(
    owner: &Arc<ComponentObservations>,
    scope: TelemetryScope,
    kind: &str,
    logging: Option<ComponentLogRuntime>,
) -> Result<PluginTelemetry> {
    let existing = owner.lifetime.with_active(|| {
        let bindings = owner.plugins.lock().map_err(|_| TelemetryError::Poisoned)?;
        match bindings.get(kind) {
            Some(binding) if binding.is_active()? => Ok(Some(binding.clone())),
            _ => Ok(None),
        }
    })?;
    if let Some(binding) = existing {
        return Ok(binding);
    }
    let lifetime = owner.lifetime.child()?;
    owner.lifetime.with_active(|| {
        let mut bindings = owner.plugins.lock().map_err(|_| TelemetryError::Poisoned)?;
        if let Some(binding) = bindings.get(kind) {
            if binding.is_active()? {
                return Ok(binding.clone());
            }
        }
        let graph = &owner.config;
        let limits = graph.plugin_limits();
        if bindings.len() >= limits.bindings && !bindings.contains_key(kind) {
            return Err(TelemetryError::Capacity("component plugin bindings"));
        }
        let config = Arc::new(PluginTelemetryConfig {
            scope,
            generation: owner.generation,
            plugin_kind: kind.into(),
            tracing: graph.policy().tracing_enabled(),
            metrics: graph.policy().metrics_enabled(),
            native_metrics_required: graph.policy().native_metrics_required(),
            trace_directives: graph.trace_directives(),
            log_directives: logging.as_ref().map_or_else(
                || "off".into(),
                |logging| logging.policy().filter().to_string(),
            ),
            metric_exclusions: graph.policy().metric_exclusions().to_vec(),
            sampling: match graph.policy().sampling() {
                RootSampling::Always => PluginRootSampling::Always,
                RootSampling::Never => PluginRootSampling::Never,
                RootSampling::Ratio(ratio) => PluginRootSampling::Ratio(ratio.value()),
            },
            limits,
        });
        config.validate()?;
        let diagnostics = TelemetryDiagnostics::default();
        let metrics = LocalMetricBinding::new(
            graph.metric_context().cloned(),
            config.clone(),
            lifetime.clone(),
            diagnostics.clone(),
        )?;
        let binding = PluginTelemetry::new(Arc::new(RuntimePluginTelemetry {
            owner: Arc::downgrade(owner),
            lifetime,
            config,
            metrics,
            diagnostics,
            logging,
            spans: graph.native_spans(),
        }));
        bindings.insert(kind.into(), binding.clone());
        Ok(binding)
    })
}

#[derive(Debug)]
struct RuntimePluginTelemetry {
    owner: Weak<ComponentObservations>,
    lifetime: TelemetryLifetime,
    config: Arc<PluginTelemetryConfig>,
    metrics: Arc<LocalMetricBinding>,
    diagnostics: TelemetryDiagnostics,
    logging: Option<ComponentLogRuntime>,
    spans: Option<NativeSpanCollector>,
}

impl RuntimePluginTelemetry {
    fn reject(&self) -> TelemetrySubmission {
        self.diagnostics.record(TelemetryLoss::Invalid);
        TelemetrySubmission::Invalid
    }
}

impl PluginTelemetryBackend for RuntimePluginTelemetry {
    fn config(&self) -> &PluginTelemetryConfig {
        &self.config
    }
    fn diagnostics(&self) -> &TelemetryDiagnostics {
        &self.diagnostics
    }
    fn for_plugin(&self, kind: &str) -> Result<PluginTelemetry> {
        let owner = self.owner.upgrade().ok_or(TelemetryError::Retired)?;
        bind(
            &owner,
            self.config.scope.clone(),
            kind,
            self.logging.clone(),
        )
    }
    fn register_metric(&self, registration: &PluginMetricRegistration) -> Result<RegisteredMetric> {
        self.metrics.register(registration)
    }
    fn register_metric_handle(
        &self,
        registration: &PluginMetricRegistration,
    ) -> Result<Option<PluginMetricHandle>> {
        self.metrics.register_handle(registration)
    }
    fn update_metric(&self, update: PluginMetricUpdate) -> Result<()> {
        self.metrics.update(update)
    }
    fn has_span_collector(&self) -> bool {
        self.spans.is_some()
    }
    fn is_active(&self) -> Result<bool> {
        self.lifetime.is_active()
    }
    fn retire(&self) -> Result<()> {
        self.lifetime.revoke(|| {}).map(|_| ())
    }
    fn submit_encoded_span(&self, bytes: &[u8]) -> TelemetrySubmission {
        if !self.config.tracing {
            return TelemetrySubmission::Accepted;
        }
        match self.lifetime.with_active(|| {
            self.spans
                .as_ref()
                .map(|spans| spans.submit(&self.config, bytes))
                .ok_or(TelemetryError::Unsupported("host native span collector"))
        }) {
            Ok(outcome) => outcome,
            Err(_) => self.reject(),
        }
    }
    fn submit_span(&self, span: &CompletedSpan) -> TelemetrySubmission {
        match encode_telemetry(span, self.config.limits.record_bytes) {
            Ok(bytes) => self.submit_encoded_span(&bytes),
            Err(_) => self.reject(),
        }
    }
    fn submit_log(&self, record: &TelemetryLogRecord) -> TelemetrySubmission {
        match self.lifetime.with_active(|| {
            record.validate()?;
            let Some(logging) = &self.logging else {
                return Ok(TelemetrySubmission::Accepted);
            };
            let scope = &self.config.scope;
            let level = match record.level {
                TelemetryLogLevel::Trace => LogLevel::Trace,
                TelemetryLogLevel::Debug => LogLevel::Debug,
                TelemetryLogLevel::Info => LogLevel::Info,
                TelemetryLogLevel::Warn => LogLevel::Warn,
                TelemetryLogLevel::Error => LogLevel::Error,
            };
            let mut message = LogMessage::with_instance(
                level,
                record.message.clone(),
                scope.instance_id(),
                scope
                    .component_id()
                    .ok_or_else(|| {
                        TelemetryError::Invalid("native log needs component identity".into())
                    })?
                    .as_str(),
                ComponentLogType::for_component(
                    scope.role().ok_or_else(|| {
                        TelemetryError::Invalid("native log needs component role".into())
                    })?,
                    scope.semantic_kind(),
                ),
            );
            let seconds = i64::try_from(record.time_unix_ns / 1_000_000_000)
                .map_err(|error| TelemetryError::Invalid(error.to_string()))?;
            let nanos = u32::try_from(record.time_unix_ns % 1_000_000_000)
                .map_err(|error| TelemetryError::Invalid(error.to_string()))?;
            message.timestamp = chrono::DateTime::from_timestamp(seconds, nanos)
                .ok_or_else(|| TelemetryError::Invalid("invalid native log timestamp".into()))?;
            message.metadata = Some(LogMetadata {
                graph_id: scope.graph_id().into(),
                plugin_kind: self.config.plugin_kind.clone(),
                generation: self.config.generation,
                target: record.target.clone(),
                attributes: record.attributes.clone(),
                context: record.context.clone(),
            });
            logging
                .try_native_log(&record.target, message)
                .map_err(|error| TelemetryError::Invalid(error.to_string()))
        }) {
            Ok(outcome) => outcome,
            Err(_) => self.reject(),
        }
    }
    fn sample_root(&self, request: &RootSpanRequest) -> Result<RootSpanDecision> {
        self.lifetime.with_active(|| {
            request.validate()?;
            match self.config.sampling {
                PluginRootSampling::Never => Ok(RootSpanDecision {
                    sampled: false,
                    trace_state: None,
                }),
                PluginRootSampling::Always => self
                    .spans
                    .as_ref()
                    .ok_or(TelemetryError::Unsupported("host native span collector"))?
                    .sample_root(&self.config, request),
                PluginRootSampling::Ratio(ratio) => {
                    let value = u64::from_be_bytes(
                        request.trace_id[..8]
                            .try_into()
                            .map_err(|_| TelemetryError::Invalid("invalid trace ID".into()))?,
                    );
                    Ok(RootSpanDecision {
                        sampled: ratio == 1.0 || (value as f64) / (u64::MAX as f64) < ratio,
                        trace_state: None,
                    })
                }
            }
        })
    }
}

impl Drop for ComponentObservations {
    fn drop(&mut self) {
        self.revoke();
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn deferred_resource_bindings_follow_resource_owner_not_component_publications() {
            let graph = GraphTelemetry::default();
            let (owner, binding) = graph.resource_binding("instance", "graph", "bootstrap", 42, None).unwrap();
            assert_eq!(binding.config().scope.component_id().unwrap().as_str(), "resource/bootstrap");
            assert_eq!(binding.config().generation, 42);
            assert!(binding.is_active().unwrap());
            let retained = owner.clone();
            drop(owner);
            assert!(binding.is_active().unwrap());
            drop(retained);
            assert!(!binding.is_active().unwrap());
            assert!(matches!(binding.for_plugin("test"), Err(TelemetryError::Retired)));
        }
    }
}
