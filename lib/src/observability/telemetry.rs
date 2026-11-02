// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Runtime-only collection settings. These never install logging, a recorder,
//! an exporter, or context propagation, and are not part of graph snapshots.

pub use drasi_computation_contracts::telemetry::facade::*;
pub use drasi_computation_contracts::telemetry::*;

use std::sync::Arc;
use tracing_subscriber::{
    filter::{filter_fn, FilterExt},
    layer::Filter,
    registry::LookupSpan,
    EnvFilter,
};

pub const TELEMETRY_TARGET: &str = "drasi::telemetry";

#[derive(Debug, Clone, Default)]
pub(crate) struct RuntimeObservability {
    pub logging: Option<super::ComponentLogRuntime>,
    pub telemetry: GraphTelemetry,
}

#[derive(Debug, Clone, Default)]
pub struct TelemetryOptions {
    settings: TelemetrySettings,
    instance_id: Option<Arc<str>>,
    metrics: Option<MetricRegistrationContext>,
    trace_directives: Option<String>,
}

impl From<TelemetrySettings> for TelemetryOptions {
    fn from(settings: TelemetrySettings) -> Self {
        Self {
            settings,
            ..Self::default()
        }
    }
}

impl From<TelemetryProfile> for TelemetryOptions {
    fn from(profile: TelemetryProfile) -> Self {
        TelemetrySettings::new(profile).into()
    }
}

impl TelemetryOptions {
    pub fn new(settings: impl Into<Self>) -> Self {
        settings.into()
    }

    pub fn with_instance_id(mut self, id: impl Into<Arc<str>>) -> Self {
        self.instance_id = Some(id.into());
        self
    }

    pub fn with_metric_context(mut self, context: MetricRegistrationContext) -> Self {
        self.metrics = Some(context);
        self
    }

    pub fn with_trace_directives(mut self, directives: impl Into<String>) -> Self {
        self.trace_directives = Some(directives.into());
        self
    }

    pub(crate) fn resolve(
        &self,
        graph_id: &str,
        instance_id: Option<&str>,
    ) -> Result<GraphTelemetry> {
        let policy = Arc::new(self.settings.resolve()?);
        if policy.metrics_enabled() && self.metrics.is_none() {
            return Err(TelemetryError::Invalid(
                "enabled metrics require a host-owned MetricRegistrationContext".into(),
            ));
        }
        if let (Some(actual), Some(requested)) = (instance_id, self.instance_id.as_deref()) {
            if actual != requested {
                return Err(TelemetryError::Invalid(
                    "telemetry cannot override the library instance identity".into(),
                ));
            }
        }
        let scope = if policy.enabled() {
            let instance_id = instance_id.or(self.instance_id.as_deref()).ok_or_else(|| {
                TelemetryError::Invalid(
                    "enabled direct-graph telemetry requires an explicit stable instance ID".into(),
                )
            })?;
            Some(TelemetryScope::graph(instance_id, graph_id)?)
        } else {
            None
        };
        let filter = EnvFilter::try_new(self.trace_directives.as_deref().unwrap_or("trace"))
            .map_err(|error| {
                TelemetryError::Invalid(format!("invalid trace directives: {error}"))
            })?;
        Ok(GraphTelemetry {
            policy,
            scope,
            metrics: self.metrics.clone(),
            filter: Arc::new(filter),
        })
    }
}

#[derive(Debug, Clone)]
pub struct GraphTelemetry {
    policy: Arc<ResolvedTelemetry>,
    scope: Option<TelemetryScope>,
    metrics: Option<MetricRegistrationContext>,
    filter: Arc<EnvFilter>,
}

impl Default for GraphTelemetry {
    fn default() -> Self {
        Self {
            policy: Arc::new(ResolvedTelemetry::default()),
            scope: None,
            metrics: None,
            filter: Arc::new(EnvFilter::new("trace")),
        }
    }
}

impl GraphTelemetry {
    pub fn policy(&self) -> &ResolvedTelemetry {
        &self.policy
    }

    pub fn scope(&self) -> Option<&TelemetryScope> {
        self.scope.as_ref()
    }

    pub fn metric_context(&self) -> Option<&MetricRegistrationContext> {
        self.metrics.as_ref()
    }

    pub(crate) fn component(
        &self,
        id: drasi_computation_contracts::ComponentId,
        role: drasi_computation_contracts::ComponentRole,
        kind: drasi_computation_contracts::ComponentSemanticKind,
    ) -> Self {
        Self {
            scope: self
                .scope
                .as_ref()
                .map(|scope| scope.component(id, role, kind)),
            ..self.clone()
        }
    }

    /// Apply only to an operation-span collection layer, never to the whole
    /// subscriber or component-log layer. This filter intentionally rejects events.
    pub fn operation_filter<S>(&self) -> impl Filter<S> + Send + Sync + 'static
    where
        S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    {
        let policy = self.policy.clone();
        filter_fn(move |metadata| {
            metadata.is_span()
                && metadata.target() == TELEMETRY_TARGET
                && policy.allows_span(metadata.name())
        })
        .and(self.filter.as_ref().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_signal_specific_requirements() {
        assert!(!TelemetryOptions::default()
            .resolve("graph", None)
            .unwrap()
            .policy()
            .enabled());
        let tracing = TelemetryOptions::new(
            TelemetrySettings::new(TelemetryProfile::Basic).with_metrics(false),
        );
        assert!(tracing.resolve("graph", None).is_err());
        let resolved = tracing
            .clone()
            .with_instance_id("host")
            .resolve("graph", None)
            .unwrap();
        assert_eq!(resolved.scope().unwrap().instance_id(), "host");
        assert!(resolved.metric_context().is_none());
        assert!(tracing.with_instance_id("").resolve("graph", None).is_err());
        assert!(TelemetryOptions::new(TelemetryProfile::Basic)
            .with_instance_id("host")
            .resolve("graph", None)
            .is_err());
    }

    #[test]
    fn library_identity_and_trace_directives_are_validated() {
        let options = TelemetryOptions::new(
            TelemetrySettings::new(TelemetryProfile::Basic).with_metrics(false),
        );
        let resolved = options
            .resolve("actual-graph", Some("actual-instance"))
            .unwrap();
        assert_eq!(resolved.scope().unwrap().graph_id(), "actual-graph");
        assert!(options
            .with_instance_id("other")
            .resolve("actual-graph", Some("actual-instance"))
            .is_err());
        assert!(TelemetryOptions::default()
            .with_trace_directives("target=not-a-level")
            .resolve("graph", None)
            .is_err());
    }
}
