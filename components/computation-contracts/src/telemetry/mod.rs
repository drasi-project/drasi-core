// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Backend-neutral observation definitions, collection policy and registration.
//! No subscriber, recorder, exporter, graph owner or native transport is installed.

mod catalogue;
mod execution;
#[cfg(feature = "tracing-context")]
mod execution_scope;
#[cfg(feature = "metrics-facade")]
pub mod facade;
#[cfg(feature = "otel-context")]
mod otel;
mod registration;

pub use catalogue::*;
pub use execution::*;
#[cfg(feature = "tracing-context")]
pub use execution_scope::*;
#[cfg(feature = "otel-context")]
pub use otel::*;
pub use registration::*;

use crate::{ComponentId, ComponentRole, ComponentSemanticKind};
use std::{str::FromStr, sync::Arc};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TelemetryError {
    #[error("invalid telemetry configuration: {0}")]
    Invalid(String),
    #[error("telemetry capability is not implemented: {0}")]
    Unsupported(&'static str),
    #[error("conflicting metric definition for '{0}'")]
    Conflict(String),
    #[error("telemetry registration capacity exhausted: {0}")]
    Capacity(&'static str),
    #[error("telemetry registration state is poisoned")]
    Poisoned,
}

pub type Result<T> = std::result::Result<T, TelemetryError>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TelemetryProfile {
    #[default]
    Off,
    Basic,
    Debug,
    Persistence,
}

impl FromStr for TelemetryProfile {
    type Err = TelemetryError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "off" => Ok(Self::Off),
            "basic" => Ok(Self::Basic),
            "debug" => Ok(Self::Debug),
            "persistence" => Ok(Self::Persistence),
            _ => Err(TelemetryError::Invalid(format!(
                "unknown profile '{value}'"
            ))),
        }
    }
}

impl TelemetryProfile {
    pub fn resolve(self) -> ResolvedTelemetry {
        ResolvedTelemetry {
            profile: self,
            traces: self != Self::Off,
            metrics: self != Self::Off,
            exclusions: Arc::from([]),
            sampling: RootSampling::Always,
            distributed_tracing: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum RootSampling {
    #[default]
    Always,
    Never,
    Ratio(SamplingRatio),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SamplingRatio(f64);

impl SamplingRatio {
    pub fn new(value: f64) -> Result<Self> {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(TelemetryError::Invalid(
                "sampling ratio must be finite and between zero and one".into(),
            ));
        }
        Ok(Self(value))
    }

    pub fn value(self) -> f64 {
        self.0
    }
}

impl RootSampling {
    /// Only a new root asks the host for a decision. A child inherits its parent.
    pub fn decide(self, parent: Option<bool>, choose_root: impl FnOnce(Self) -> bool) -> bool {
        parent.unwrap_or_else(|| match self {
            Self::Always => true,
            Self::Never => false,
            Self::Ratio(_) => choose_root(self),
        })
    }
}

#[derive(Debug, Clone)]
pub struct TelemetrySettings {
    profile: TelemetryProfile,
    traces: bool,
    metrics: bool,
    exclusions: Vec<String>,
    sampling: RootSampling,
    distributed_tracing: bool,
    native_metrics: bool,
}

impl Default for TelemetrySettings {
    fn default() -> Self {
        Self::new(TelemetryProfile::Off)
    }
}

impl TelemetrySettings {
    pub fn new(profile: TelemetryProfile) -> Self {
        Self {
            profile,
            traces: true,
            metrics: true,
            exclusions: Vec::new(),
            sampling: RootSampling::Always,
            distributed_tracing: false,
            native_metrics: false,
        }
    }

    pub fn with_tracing(mut self, enabled: bool) -> Self {
        self.traces = enabled;
        self
    }

    pub fn with_metrics(mut self, enabled: bool) -> Self {
        self.metrics = enabled;
        self
    }

    /// Literal substrings; `*` is not a wildcard. An empty substring excludes all.
    pub fn exclude_metrics(mut self, substring: impl Into<String>) -> Self {
        self.exclusions.push(substring.into());
        self
    }

    pub fn with_sampling(mut self, sampling: RootSampling) -> Self {
        self.sampling = sampling;
        self
    }

    pub fn require_distributed_tracing(mut self) -> Self {
        self.distributed_tracing = true;
        self
    }

    pub fn require_native_metrics(mut self) -> Self {
        self.native_metrics = true;
        self
    }

    pub fn resolve(&self) -> Result<ResolvedTelemetry> {
        if self.native_metrics {
            return Err(TelemetryError::Unsupported("native metric callbacks"));
        }
        if self.exclusions.len() > 64 || self.exclusions.iter().any(|value| value.len() > 256) {
            return Err(TelemetryError::Invalid(
                "at most 64 metric exclusions of 256 bytes each are supported".into(),
            ));
        }
        let mut policy = self.profile.resolve();
        policy.traces &= self.traces;
        policy.metrics &= self.metrics;
        policy.exclusions = self.exclusions.clone().into();
        policy.sampling = self.sampling;
        policy.distributed_tracing = self.distributed_tracing;
        Ok(policy)
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedTelemetry {
    profile: TelemetryProfile,
    traces: bool,
    metrics: bool,
    exclusions: Arc<[String]>,
    sampling: RootSampling,
    distributed_tracing: bool,
}

impl Default for ResolvedTelemetry {
    fn default() -> Self {
        TelemetryProfile::Off.resolve()
    }
}

impl ResolvedTelemetry {
    pub fn distributed_tracing_required(&self) -> bool {
        self.distributed_tracing
    }

    pub fn profile(&self) -> TelemetryProfile {
        self.profile
    }

    pub fn tracing_enabled(&self) -> bool {
        self.traces
    }

    pub fn metrics_enabled(&self) -> bool {
        self.metrics
    }

    pub fn enabled(&self) -> bool {
        self.traces || self.metrics
    }

    pub fn sampling(&self) -> RootSampling {
        self.sampling
    }

    pub fn allows_detail(&self, detail: ObservationDetail) -> bool {
        match detail {
            ObservationDetail::Basic => self.profile != TelemetryProfile::Off,
            ObservationDetail::Debug => self.profile == TelemetryProfile::Debug,
            ObservationDetail::Persistence => self.profile == TelemetryProfile::Persistence,
            ObservationDetail::Deferred => false,
        }
    }

    pub fn allows_span(&self, name: &str) -> bool {
        self.traces
            && SPANS
                .iter()
                .find(|span| span.name == name)
                .is_some_and(|span| self.allows_detail(span.detail))
    }

    pub fn allows_metric(&self, name: &str) -> bool {
        self.metrics
            && !self.exclusions.iter().any(|part| name.contains(part))
            && METRICS
                .iter()
                .find(|metric| metric.name == name)
                .map_or(true, |metric| self.allows_detail(metric.detail))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryScope {
    instance_id: Arc<str>,
    graph_id: Arc<str>,
    component: Option<(ComponentId, ComponentRole, ComponentSemanticKind)>,
}

impl TelemetryScope {
    pub fn graph(instance_id: impl Into<Arc<str>>, graph_id: impl Into<Arc<str>>) -> Result<Self> {
        let instance_id = instance_id.into();
        let graph_id = graph_id.into();
        for value in [&instance_id, &graph_id] {
            crate::validate_identifier("telemetry scope", value)
                .map_err(|error| TelemetryError::Invalid(error.to_string()))?;
        }
        Ok(Self {
            instance_id,
            graph_id,
            component: None,
        })
    }

    pub fn component(
        &self,
        id: ComponentId,
        role: ComponentRole,
        kind: ComponentSemanticKind,
    ) -> Self {
        Self {
            instance_id: self.instance_id.clone(),
            graph_id: self.graph_id.clone(),
            component: Some((id, role, kind)),
        }
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub fn graph_id(&self) -> &str {
        &self.graph_id
    }

    pub fn component_id(&self) -> Option<&ComponentId> {
        self.component.as_ref().map(|(id, _, _)| id)
    }

    pub fn role(&self) -> Option<ComponentRole> {
        self.component.as_ref().map(|(_, role, _)| *role)
    }

    pub fn semantic_kind(&self) -> Option<ComponentSemanticKind> {
        self.component.as_ref().map(|(_, _, kind)| *kind)
    }

    pub fn labels(&self) -> Vec<(String, String)> {
        let mut labels = vec![
            ("instance_id".into(), self.instance_id.to_string()),
            ("graph_id".into(), self.graph_id.to_string()),
        ];
        if let Some((id, _, kind)) = &self.component {
            let kind = match kind {
                ComponentSemanticKind::Source => "source",
                ComponentSemanticKind::Transformer => "transformer",
                ComponentSemanticKind::Query => "query",
                ComponentSemanticKind::Sink => "sink",
                ComponentSemanticKind::Reaction => "reaction",
                ComponentSemanticKind::Service => "service",
            };
            labels.push(("component_id".into(), id.to_string()));
            labels.push(("component_kind".into(), kind.into()));
        }
        labels
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn profile_and_signal_matrix() {
        for profile in [
            TelemetryProfile::Off,
            TelemetryProfile::Basic,
            TelemetryProfile::Debug,
            TelemetryProfile::Persistence,
        ] {
            for traces in [false, true] {
                for metrics in [false, true] {
                    let policy = TelemetrySettings::new(profile)
                        .with_tracing(traces)
                        .with_metrics(metrics)
                        .resolve()
                        .unwrap();
                    assert_eq!(
                        policy.tracing_enabled(),
                        traces && profile != TelemetryProfile::Off
                    );
                    assert_eq!(
                        policy.metrics_enabled(),
                        metrics && profile != TelemetryProfile::Off
                    );
                    assert_eq!(
                        policy.allows_span("query.evaluate"),
                        traces && profile == TelemetryProfile::Debug
                    );
                    assert!(!policy.allows_metric("drasi.pipeline.end_to_end.duration"));
                }
            }
        }
        assert!("invalid".parse::<TelemetryProfile>().is_err());
    }

    #[test]
    fn exclusions_are_literal_and_cannot_enable_collection() {
        let policy = TelemetrySettings::new(TelemetryProfile::Basic)
            .exclude_metrics("query.")
            .exclude_metrics("queue.*")
            .resolve()
            .unwrap();
        assert!(!policy.allows_metric("drasi.query.events_processed"));
        assert!(policy.allows_metric("drasi.queue.depth"));
        assert!(!TelemetrySettings::default()
            .resolve()
            .unwrap()
            .allows_metric("drasi.errors"));
        assert!(!TelemetrySettings::new(TelemetryProfile::Debug)
            .exclude_metrics("")
            .resolve()
            .unwrap()
            .allows_metric("drasi.errors"));
    }

    #[test]
    fn sampling_inherits_without_consulting_root_policy() {
        let sampling = RootSampling::Ratio(SamplingRatio::new(0.5).unwrap());
        for parent in [false, true] {
            assert_eq!(
                sampling.decide(Some(parent), |_| panic!("child sampled twice")),
                parent
            );
        }
        let calls = Cell::new(0);
        let decision = sampling.decide(None, |_| {
            calls.set(calls.get() + 1);
            true
        });
        assert!(decision);
        assert_eq!(calls.get(), 1);
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.1, 1.1] {
            assert!(SamplingRatio::new(invalid).is_err());
        }
        for valid in [0.0, 1.0] {
            assert_eq!(SamplingRatio::new(valid).unwrap().value(), valid);
        }
        let metric = "drasi.component.calls";
        for sampling in [RootSampling::Always, RootSampling::Never, sampling] {
            for traces in [false, true] {
                assert!(TelemetrySettings::new(TelemetryProfile::Basic)
                    .with_tracing(traces)
                    .with_sampling(sampling)
                    .resolve()
                    .unwrap()
                    .allows_metric(metric));
            }
        }
    }

    #[test]
    fn required_context_is_explicit_and_native_metrics_remain_unsupported() {
        assert!(TelemetrySettings::default()
            .require_distributed_tracing()
            .resolve()
            .unwrap()
            .distributed_tracing_required());
        assert!(matches!(
            TelemetrySettings::default()
                .require_native_metrics()
                .resolve(),
            Err(TelemetryError::Unsupported(_))
        ));
    }
}
