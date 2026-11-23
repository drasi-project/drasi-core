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

#[derive(Debug, Clone)]
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

/// DrasiLib collects the `Basic` profile unless the host selects another one.
impl Default for TelemetryOptions {
    fn default() -> Self {
        Self {
            settings: TelemetrySettings::new(TelemetryProfile::Basic),
            instance_id: None,
            metrics: None,
            trace_directives: None,
            context_adapter: None,
            link_limits: TraceLinkLimits::default(),
            native_spans: None,
            plugin_limits: TelemetryLimits::default(),
        }
    }
}

/// Forwards to the recorder current at registration: thread-local, then
/// global, otherwise a no-op. Drasi never installs one.
struct CurrentRecorder;

impl metrics::Recorder for CurrentRecorder {
    fn describe_counter(
        &self,
        key: metrics::KeyName,
        unit: Option<metrics::Unit>,
        description: metrics::SharedString,
    ) {
        metrics::with_recorder(|recorder| recorder.describe_counter(key, unit, description));
    }

    fn describe_gauge(
        &self,
        key: metrics::KeyName,
        unit: Option<metrics::Unit>,
        description: metrics::SharedString,
    ) {
        metrics::with_recorder(|recorder| recorder.describe_gauge(key, unit, description));
    }

    fn describe_histogram(
        &self,
        key: metrics::KeyName,
        unit: Option<metrics::Unit>,
        description: metrics::SharedString,
    ) {
        metrics::with_recorder(|recorder| recorder.describe_histogram(key, unit, description));
    }

    fn register_counter(
        &self,
        key: &metrics::Key,
        metadata: &metrics::Metadata<'_>,
    ) -> metrics::Counter {
        metrics::with_recorder(|recorder| recorder.register_counter(key, metadata))
    }

    fn register_gauge(
        &self,
        key: &metrics::Key,
        metadata: &metrics::Metadata<'_>,
    ) -> metrics::Gauge {
        metrics::with_recorder(|recorder| recorder.register_gauge(key, metadata))
    }

    fn register_histogram(
        &self,
        key: &metrics::Key,
        metadata: &metrics::Metadata<'_>,
    ) -> metrics::Histogram {
        metrics::with_recorder(|recorder| recorder.register_histogram(key, metadata))
    }
}

/// One process-wide context, so instances without a host context share one
/// registry for the current recorder.
fn implicit_metric_context() -> MetricRegistrationContext {
    static CONTEXT: std::sync::OnceLock<MetricRegistrationContext> = std::sync::OnceLock::new();
    CONTEXT
        .get_or_init(|| {
            MetricRegistrationContext::new(Arc::new(CurrentRecorder), RegistrationLimits::default())
                .expect("default registration limits are valid")
        })
        .clone()
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
        self.resolve_with_implicit_metrics(graph_id, instance_id, implicit_metric_context)
    }

    /// `implicit` supplies the context used when metrics are enabled without a
    /// host-owned one.
    pub(crate) fn resolve_with_implicit_metrics(
        &self,
        graph_id: &str,
        instance_id: Option<&str>,
        implicit: impl FnOnce() -> MetricRegistrationContext,
    ) -> Result<GraphTelemetry> {
        let policy = Arc::new(self.settings.resolve()?);
        let link_limits = self.link_limits.validate()?;
        let plugin_limits = self.plugin_limits.validate()?;
        if policy.distributed_tracing_required() && self.context_adapter.is_none() {
            return Err(TelemetryError::Unsupported(
                "distributed trace context adapter",
            ));
        }
        let implicit_metrics = self.metrics.is_none() && policy.metrics_enabled();
        let metrics = match &self.metrics {
            Some(context) => Some(context.clone()),
            None if implicit_metrics => Some(implicit()),
            None => None,
        };
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
            drasi_computation_contracts::validate_identifier("telemetry instance ID", instance_id)
                .map_err(|_| {
                    TelemetryError::Invalid(format!(
                        "telemetry requires a valid instance ID, but '{instance_id}' is empty or \
                         contains whitespace or control characters; change the ID or disable \
                         telemetry with TelemetryProfile::Off"
                    ))
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
            metrics,
            implicit_metrics,
            filter: Arc::new(filter),
            context_adapter: self.context_adapter.clone(),
            link_limits,
            native_spans: self.native_spans.clone(),
            plugin_limits,
            handled_branch,
            owners: Arc::new(Mutex::new(BTreeMap::new())),
            component_names: Arc::default(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct GraphTelemetry {
    policy: Arc<ResolvedTelemetry>,
    scope: Option<TelemetryScope>,
    metrics: Option<MetricRegistrationContext>,
    /// The shared implicit context, which skips registrations at capacity
    /// rather than failing components the host never asked to measure.
    implicit_metrics: bool,
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
    component_names: Arc<Mutex<ComponentNames>>,
}

/// `drasi.component.info` series: the current name gauge per component, plus
/// the components and queue edges already warned about a failed registration.
#[derive(Debug, Default)]
struct ComponentNames {
    current: BTreeMap<drasi_computation_contracts::ComponentId, (String, Option<metrics::Gauge>)>,
    warned: std::collections::BTreeSet<drasi_computation_contracts::ComponentId>,
    warned_edges: std::collections::BTreeSet<(String, String, String)>,
}

// Teardown paths that never dispose the graph still retire its names once the
// last telemetry handle sharing this registry is gone.
impl Drop for ComponentNames {
    fn drop(&mut self) {
        for (_, gauge) in self.current.values() {
            if let Some(gauge) = gauge {
                gauge.set(0.0);
            }
        }
    }
}

impl Default for GraphTelemetry {
    fn default() -> Self {
        Self {
            policy: Arc::new(ResolvedTelemetry::default()),
            scope: None,
            metrics: None,
            implicit_metrics: false,
            filter: Arc::new(EnvFilter::new("trace")),
            context_adapter: None,
            link_limits: TraceLinkLimits::default(),
            native_spans: None,
            plugin_limits: TelemetryLimits::default(),
            handled_branch: None,
            owners: Arc::new(Mutex::new(BTreeMap::new())),
            component_names: Arc::default(),
        }
    }
}

impl GraphTelemetry {
    /// Publishes each component's configured name as `drasi.component.info`:
    /// one for the current name, zero for a name retired by rename or removal.
    /// Names are metadata, so a registration failure is logged once per
    /// component and never fails the graph operation that changed the name.
    pub(crate) fn sync_component_names(
        &self,
        components: impl IntoIterator<
            Item = (
                drasi_computation_contracts::ComponentId,
                drasi_computation_contracts::ComponentRole,
                drasi_computation_contracts::ComponentSemanticKind,
                String,
            ),
        >,
    ) {
        let (Some(context), Some(scope)) = (&self.metrics, &self.scope) else {
            return;
        };
        let Ok(mut names) = self.component_names.lock() else {
            log::error!("Component name telemetry state poisoned");
            return;
        };
        let mut present = std::collections::BTreeSet::new();
        for (id, role, kind, name) in components {
            present.insert(id.clone());
            if names
                .current
                .get(&id)
                .is_some_and(|(current, _)| *current == name)
            {
                continue;
            }
            if let Some((_, Some(retired))) = names.current.remove(&id) {
                retired.set(0.0);
            }
            let registered = context
                .framework(
                    &self.policy,
                    &scope.component(id.clone(), role, kind),
                    "drasi.component.info",
                    [("component_name".to_string(), name.clone())],
                )
                .and_then(RegisteredMetric::gauge);
            let gauge = match registered {
                Ok(gauge) => {
                    gauge.set(1.0);
                    Some(gauge)
                }
                Err(error) => {
                    if names.warned.insert(id.clone()) {
                        log::warn!("Omitting drasi.component.info for component {id}: {error}");
                    }
                    None
                }
            };
            names.current.insert(id, (name, gauge));
        }
        names.current.retain(|id, (_, gauge)| {
            let keep = present.contains(id);
            if let (false, Some(gauge)) = (keep, gauge) {
                gauge.set(0.0);
            }
            keep
        });
    }

    /// Registers `drasi.queue.edge_info` for one graph edge bound to a physical
    /// queue and sets it to one. Like component names, this is metadata: a
    /// registration failure is logged once per edge and yields a no-op gauge.
    pub(crate) fn queue_edge_info(&self, queue_id: &str, from: &str, to: &str) -> metrics::Gauge {
        let registered = self
            .register(
                "drasi.queue.edge_info",
                vec![
                    ("queue_id".into(), queue_id.into()),
                    ("from_component_id".into(), from.into()),
                    ("to_component_id".into(), to.into()),
                ],
            )
            .and_then(RegisteredMetric::gauge);
        match registered {
            Ok(gauge) => {
                gauge.set(1.0);
                gauge
            }
            Err(error) => {
                let first = self
                    .component_names
                    .lock()
                    .map(|mut names| {
                        names
                            .warned_edges
                            .insert((queue_id.into(), from.into(), to.into()))
                    })
                    .unwrap_or(true);
                if first {
                    log::warn!(
                        "Omitting drasi.queue.edge_info for queue {queue_id} ({from} -> {to}): {error}"
                    );
                }
                metrics::Gauge::noop()
            }
        }
    }
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
            (Some(context), Some(scope)) => {
                match context.framework(&self.policy, scope, name, labels) {
                    Err(TelemetryError::Capacity(limit)) if self.implicit_metrics => {
                        static WARNED: std::sync::atomic::AtomicBool =
                            std::sync::atomic::AtomicBool::new(false);
                        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                            log::warn!(
                                "Default telemetry metric registration reached its {limit} \
                                 limit; new metrics are not recorded. Supply \
                                 TelemetryOptions::with_metric_context with larger \
                                 RegistrationLimits to record them."
                            );
                        }
                        Ok(RegisteredMetric::Disabled)
                    }
                    result => result,
                }
            }
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
                        && readiness.is_none_or(|plane| plane.is_ready(id, component.generation)),
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
            .unwrap()
            .metric_context()
            .is_some());
    }

    #[test]
    fn default_options_are_basic_and_off_is_explicit() {
        let default = TelemetryOptions::default()
            .resolve("graph", Some("host"))
            .unwrap();
        assert!(default.policy().enabled());
        assert!(default.policy().metrics_enabled());
        let off = TelemetryOptions::new(TelemetryProfile::Off)
            .resolve("graph", Some("host"))
            .unwrap();
        assert!(!off.policy().enabled());
        assert!(off.scope().is_none() && off.metric_context().is_none());
    }

    #[test]
    fn host_context_overrides_the_implicit_one() {
        let host =
            MetricRegistrationContext::new(Arc::new(metrics::NoopRecorder), Default::default())
                .unwrap();
        let resolved = TelemetryOptions::default()
            .with_metric_context(host.clone())
            .resolve("graph", Some("host"))
            .unwrap();
        assert!(resolved.policy().metrics_enabled());
        assert!(std::ptr::eq(
            resolved.metric_context().unwrap().registrations(),
            host.registrations()
        ));
    }

    #[test]
    fn only_the_implicit_context_degrades_when_full() {
        use drasi_computation_contracts::{ComponentRole, ComponentSemanticKind};
        let tiny = || {
            MetricRegistrationContext::new(
                Arc::new(metrics::NoopRecorder),
                RegistrationLimits {
                    series: 1,
                    ..RegistrationLimits::default()
                },
            )
            .unwrap()
        };
        let register = |graph: &GraphTelemetry, name: &str| {
            graph
                .component(
                    crate::test_helpers::named_component_id(name),
                    ComponentRole::Source,
                    ComponentSemanticKind::Source,
                )
                .register("drasi.component.up", Vec::new())
        };
        let implicit = TelemetryOptions::default()
            .resolve_with_implicit_metrics("graph", Some("host"), tiny)
            .unwrap();
        assert!(register(&implicit, "a").is_ok());
        assert!(matches!(
            register(&implicit, "b"),
            Ok(RegisteredMetric::Disabled)
        ));
        let host = TelemetryOptions::default()
            .with_metric_context(tiny())
            .resolve("graph", Some("host"))
            .unwrap();
        assert!(register(&host, "a").is_ok());
        assert!(matches!(
            register(&host, "b"),
            Err(TelemetryError::Capacity(_))
        ));
    }

    #[test]
    fn invalid_instance_id_names_the_id_and_the_opt_out() {
        let options = TelemetryOptions::new(
            TelemetrySettings::new(TelemetryProfile::Basic).with_metrics(false),
        );
        let error = options
            .resolve("graph", Some("my server"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("'my server'"), "{error}");
        assert!(error.contains("TelemetryProfile::Off"), "{error}");
        assert!(TelemetryOptions::new(TelemetryProfile::Off)
            .resolve("graph", Some("my server"))
            .is_ok());
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
        assert!(TelemetryOptions::new(TelemetryProfile::Off)
            .with_trace_directives("target=not-a-level")
            .resolve("graph", None)
            .is_err());
    }
}
