// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Runtime-only collection settings. These never install logging, a recorder,
//! an exporter, or a global context adapter, and are not part of graph snapshots.

pub use drasi_computation_contracts::telemetry::facade::*;
pub use drasi_computation_contracts::telemetry::*;

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
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
    context_adapter: Option<Arc<dyn TraceContextAdapter>>,
    link_limits: TraceLinkLimits,
    native_spans: Option<super::NativeSpanCollector>,
    plugin_limits: TelemetryLimits,
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
    pub fn with_native_span_collector(mut self, collector: super::NativeSpanCollector) -> Self {
        self.plugin_limits = collector.limits();
        self.native_spans = Some(collector);
        self
    }

    pub fn with_plugin_telemetry_limits(mut self, limits: TelemetryLimits) -> Self {
        self.plugin_limits = limits;
        self
    }
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

    pub fn with_context_adapter(mut self, adapter: Arc<dyn TraceContextAdapter>) -> Self {
        self.context_adapter = Some(adapter);
        self
    }

    pub fn with_trace_link_limits(mut self, limits: TraceLinkLimits) -> Self {
        self.link_limits = limits;
        self
    }

    pub(crate) fn resolve(
        &self,
        graph_id: &str,
        instance_id: Option<&str>,
    ) -> Result<GraphTelemetry> {
        let policy = Arc::new(self.settings.resolve()?);
        let link_limits = self.link_limits.validate()?;
        let plugin_limits = self.plugin_limits.validate()?;
        if policy.distributed_tracing_required() && self.context_adapter.is_none() {
            return Err(TelemetryError::Unsupported(
                "distributed trace context adapter",
            ));
        }
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
        let handled_branch = policy
            .handled_branch_limits()
            .map(HandledBranchTracker::new)
            .transpose()?;
        Ok(GraphTelemetry {
            policy,
            scope,
            metrics: self.metrics.clone(),
            filter: Arc::new(filter),
            context_adapter: self.context_adapter.clone(),
            link_limits,
            native_spans: self.native_spans.clone(),
            plugin_limits,
            handled_branch,
            owners: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }
}

#[derive(Debug, Clone)]
pub struct GraphTelemetry {
    policy: Arc<ResolvedTelemetry>,
    scope: Option<TelemetryScope>,
    metrics: Option<MetricRegistrationContext>,
    filter: Arc<EnvFilter>,
    context_adapter: Option<Arc<dyn TraceContextAdapter>>,
    link_limits: TraceLinkLimits,
    native_spans: Option<super::NativeSpanCollector>,
    plugin_limits: TelemetryLimits,
    handled_branch: Option<Arc<HandledBranchTracker>>,
    owners: Arc<
        Mutex<
            BTreeMap<drasi_computation_contracts::ComponentId, Arc<super::ComponentObservations>>,
        >,
    >,
}

impl Default for GraphTelemetry {
    fn default() -> Self {
        Self {
            policy: Arc::new(ResolvedTelemetry::default()),
            scope: None,
            metrics: None,
            filter: Arc::new(EnvFilter::new("trace")),
            context_adapter: None,
            link_limits: TraceLinkLimits::default(),
            native_spans: None,
            plugin_limits: TelemetryLimits::default(),
            handled_branch: None,
            owners: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
}

impl GraphTelemetry {
    pub fn handled_branch_diagnostics(&self) -> Result<Option<HandledBranchDiagnostics>> {
        self.handled_branch
            .as_ref()
            .map(|tracker| tracker.diagnostics())
            .transpose()
    }

    pub(crate) fn handled_branch_tracker(&self) -> Option<&Arc<HandledBranchTracker>> {
        self.handled_branch.as_ref()
    }

    pub(crate) fn resource_binding(
        &self,
        instance: &str,
        graph: &str,
        resource: &str,
        generation: u64,
        logging: Option<super::ComponentLogRuntime>,
    ) -> Result<(Arc<super::ComponentObservations>, PluginTelemetry)> {
        use drasi_computation_contracts::ComponentId;
        // A separate stable namespace identifies resource telemetry, not graph nodes.
        let namespace = ComponentId::from_uuid(uuid::Uuid::from_u128(
            0x4995fdd4_34d3_4adb_b9c9_14484e4d6e20,
        ));
        let scope = TelemetryScope::graph(instance, graph)?.component(
            ComponentId::derive_owned(&namespace, resource)
                .map_err(|error| TelemetryError::Invalid(error.to_string()))?,
            drasi_computation_contracts::ComponentRole::Service,
            drasi_computation_contracts::ComponentSemanticKind::Service,
        );
        let mut config = self.clone();
        config.owners = Arc::new(Mutex::new(BTreeMap::new()));
        // Resource lifetimes use the shared fence, without registering synthetic
        // component calls/up/queue measurements or publishing another graph node.
        let mut owner = super::ComponentObservations::new(GraphTelemetry::default(), generation)?;
        Arc::get_mut(&mut owner)
            .ok_or_else(|| {
                TelemetryError::Invalid("resource telemetry owner is unexpectedly shared".into())
            })?
            .config = config;
        let binding = super::plugin_telemetry::bind(&owner, scope, "resource", logging)?;
        Ok((owner, binding))
    }

    pub fn plugin_binding(
        &self,
        scope: TelemetryScope,
        generation: u64,
        kind: &str,
        logging: Option<super::ComponentLogRuntime>,
    ) -> Result<PluginTelemetry> {
        let owner = self.observations(
            scope.component_id().cloned().ok_or_else(|| {
                TelemetryError::Invalid("plugin binding needs a component".into())
            })?,
            scope
                .role()
                .ok_or_else(|| TelemetryError::Invalid("plugin binding needs a role".into()))?,
            scope.semantic_kind().ok_or_else(|| {
                TelemetryError::Invalid("plugin binding needs a semantic kind".into())
            })?,
            generation,
        )?;
        super::plugin_telemetry::bind(&owner, scope, kind, logging)
    }

    pub(super) fn native_spans(&self) -> Option<super::NativeSpanCollector> {
        self.native_spans.clone()
    }
    pub(super) fn plugin_limits(&self) -> TelemetryLimits {
        self.plugin_limits
    }
    pub(super) fn trace_directives(&self) -> String {
        self.filter.to_string()
    }
    pub(crate) fn link_limits(&self) -> TraceLinkLimits {
        self.link_limits
    }

    pub(crate) fn register(
        &self,
        name: &str,
        labels: Vec<(String, String)>,
    ) -> Result<RegisteredMetric> {
        match (&self.metrics, &self.scope) {
            (Some(context), Some(scope)) => context.framework(&self.policy, scope, name, labels),
            _ => Ok(RegisteredMetric::Disabled),
        }
    }

    pub(crate) fn observations(
        &self,
        id: drasi_computation_contracts::ComponentId,
        role: drasi_computation_contracts::ComponentRole,
        kind: drasi_computation_contracts::ComponentSemanticKind,
        generation: u64,
    ) -> Result<Arc<super::ComponentObservations>> {
        let mut owners = self.owners.lock().map_err(|_| TelemetryError::Poisoned)?;
        if let Some(owner) = owners
            .get(&id)
            .filter(|owner| owner.generation == generation)
        {
            return Ok(owner.clone());
        }
        let mut config = self.component(id.clone(), role, kind);
        // A binding must not retain the cache which owns it.
        config.owners = Arc::new(Mutex::new(BTreeMap::new()));
        let owner = super::ComponentObservations::new(config, generation)?;
        if let Some(previous) = owners.insert(id, owner.clone()) {
            previous.revoke();
        }
        Ok(owner)
    }

    pub(crate) fn publish_components(
        &self,
        observed: &crate::computation::v1::ObservedGraph,
        readiness: Option<&crate::computation::v1::ControlPlane>,
    ) {
        let mut owners = self.owners.lock().unwrap_or_else(|error| {
            log::error!("Publishing poisoned telemetry bindings: {error}");
            error.into_inner()
        });
        owners.retain(|id, owner| {
            let current = observed
                .components
                .get(id)
                .filter(|component| component.generation.0 == owner.generation);
            if let Some(component) = current {
                owner.update_up(
                    component.lifecycle == crate::computation::v1::ComponentLifecycle::Running
                        && component.started
                        && !component.exhausted
                        && component.failure.is_none()
                        && readiness.map_or(true, |plane| plane.is_ready(id, component.generation)),
                );
                true
            } else {
                owner.revoke();
                false
            }
        });
    }

    pub fn context_adapter(&self) -> Option<&Arc<dyn TraceContextAdapter>> {
        self.context_adapter.as_ref()
    }

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
                && ((metadata.target() == TELEMETRY_TARGET && policy.allows_span(metadata.name()))
                    || (metadata.target() != TELEMETRY_TARGET
                        && metadata.name() != "computation_component"
                        && policy.tracing_enabled()
                        && ExecutionScope::current_plugin_telemetry()
                            .is_some_and(|binding| binding.config().tracing)))
        })
        .and(self.filter.as_ref().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_signal_specific_requirements() {
        assert!(matches!(
            TelemetryOptions::new(TelemetrySettings::default().require_distributed_tracing())
                .resolve("graph", None),
            Err(TelemetryError::Unsupported(_))
        ));
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
