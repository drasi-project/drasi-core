// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Opt-in, backend-neutral facade adapter shared by the library and static SDK.
//! The host supplies the recorder and shares one context per recorder. This
//! module installs no globals and is not a native-plugin callback transport.

use super::{
    MetricKind, MetricRegistration, MetricRegistrationGuard, MetricRegistrationState, MetricUnit,
    RegistrationLimits, ResolvedTelemetry, Result, TelemetryError, TelemetryScope,
};
use metrics::{Counter, Gauge, Histogram, Key, Label, Level, Metadata, Recorder, Unit};
use std::sync::Arc;

const METADATA: Metadata<'static> = Metadata::new("drasi", Level::INFO, Some(module_path!()));

#[derive(Debug)]
pub struct PluginMetricDefinition<'a> {
    pub leaf: &'a str,
    pub kind: MetricKind,
    pub unit: MetricUnit,
    pub description: &'a str,
}

#[derive(Debug, Clone)]
pub enum RegisteredMetric {
    Disabled,
    Counter(Counter),
    Gauge(Gauge),
    Histogram(Histogram),
}

impl RegisteredMetric {
    pub fn counter(self) -> Result<Counter> {
        match self {
            Self::Disabled => Ok(Counter::noop()),
            Self::Counter(handle) => Ok(handle),
            _ => Err(TelemetryError::Invalid(
                "registered metric is not a counter".into(),
            )),
        }
    }

    pub fn gauge(self) -> Result<Gauge> {
        match self {
            Self::Disabled => Ok(Gauge::noop()),
            Self::Gauge(handle) => Ok(handle),
            _ => Err(TelemetryError::Invalid(
                "registered metric is not a gauge".into(),
            )),
        }
    }

    pub fn histogram(self) -> Result<Histogram> {
        match self {
            Self::Disabled => Ok(Histogram::noop()),
            Self::Histogram(handle) => Ok(handle),
            _ => Err(TelemetryError::Invalid(
                "registered metric is not a histogram".into(),
            )),
        }
    }
}

struct Context {
    recorder: Arc<dyn Recorder + Send + Sync>,
    registrations: MetricRegistrationState,
}

/// Clone this context into every graph and static SDK integration using its
/// recorder. Constructing another context is not a way to bypass conflicts.
/// Raw registration retains recorder semantics. Component code uses
/// `ScopedMetricEmitter` for generation-local accounting and revocation.
/// An optional host guard applies at the same shared acceptance boundary for
/// framework metrics and scoped plugin registrations, including native handles.
#[derive(Clone)]
pub struct MetricRegistrationContext(Arc<Context>);

impl std::fmt::Debug for MetricRegistrationContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MetricRegistrationContext")
            .field("registrations", &self.0.registrations)
            .finish_non_exhaustive()
    }
}

impl MetricRegistrationContext {
    /// Creates a context with built-in validation only and no host guard.
    pub fn new(
        recorder: Arc<dyn Recorder + Send + Sync>,
        limits: RegistrationLimits,
    ) -> Result<Self> {
        Ok(Self(Arc::new(Context {
            recorder,
            registrations: MetricRegistrationState::new(limits)?,
        })))
    }

    /// Creates a context with a host-owned acceptance policy. Guard rejection
    /// leaves registration capacity unchanged and invokes no recorder callbacks.
    /// The guard runs under the registry lock as specified by
    /// [`MetricRegistrationGuard`]; recorder callbacks run only after release.
    pub fn new_with_guard(
        recorder: Arc<dyn Recorder + Send + Sync>,
        limits: RegistrationLimits,
        guard: Arc<dyn MetricRegistrationGuard>,
    ) -> Result<Self> {
        Ok(Self(Arc::new(Context {
            recorder,
            registrations: MetricRegistrationState::new_with_guard(limits, guard)?,
        })))
    }

    pub fn registrations(&self) -> &MetricRegistrationState {
        &self.0.registrations
    }

    pub fn framework(
        &self,
        policy: &ResolvedTelemetry,
        scope: &TelemetryScope,
        name: &str,
        labels: impl IntoIterator<Item = (String, String)>,
    ) -> Result<RegisteredMetric> {
        if !policy.allows_metric(name) {
            return Ok(RegisteredMetric::Disabled);
        }
        self.register(MetricRegistration::framework(name, scope, labels)?)
    }

    pub fn plugin(
        &self,
        policy: &ResolvedTelemetry,
        scope: &TelemetryScope,
        plugin_kind: &str,
        definition: PluginMetricDefinition<'_>,
        labels: impl IntoIterator<Item = (String, String)>,
    ) -> Result<RegisteredMetric> {
        let name = format!("drasi.plugin.{plugin_kind}.{}", definition.leaf);
        if !policy.allows_metric(&name) {
            return Ok(RegisteredMetric::Disabled);
        }
        self.register(MetricRegistration::plugin(
            scope,
            plugin_kind,
            definition.leaf,
            definition.kind,
            definition.unit,
            definition.description,
            labels,
        )?)
    }

    pub(super) fn register(&self, registration: MetricRegistration) -> Result<RegisteredMetric> {
        self.0.registrations.register(&registration)?;
        let name = registration.name().to_owned();
        let key = Key::from_parts(
            name.clone(),
            registration
                .labels()
                .iter()
                .map(|(key, value)| Label::new(key.clone(), value.clone()))
                .collect::<Vec<_>>(),
        );
        let unit = match registration.unit() {
            MetricUnit::One => Unit::Count,
            MetricUnit::Seconds => Unit::Seconds,
        };
        let description = registration.description().to_owned().into();
        let recorder = &self.0.recorder;
        Ok(match registration.kind() {
            MetricKind::Counter => {
                recorder.describe_counter(name.into(), Some(unit), description);
                RegisteredMetric::Counter(recorder.register_counter(&key, &METADATA))
            }
            MetricKind::Gauge => {
                recorder.describe_gauge(name.into(), Some(unit), description);
                RegisteredMetric::Gauge(recorder.register_gauge(&key, &METADATA))
            }
            MetricKind::Histogram => {
                recorder.describe_histogram(name.into(), Some(unit), description);
                RegisteredMetric::Histogram(recorder.register_histogram(&key, &METADATA))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        PluginMetricRegistration, RootSampling, TelemetryProfile, TelemetrySettings,
    };
    use super::*;
    use crate::{ComponentId, ComponentRole, ComponentSemanticKind};
    use metrics::{CounterFn, GaugeFn, HistogramFn, KeyName, SharedString};
    use std::sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Mutex,
    };

    #[derive(Default)]
    struct Observations {
        counter: AtomicU64,
        gauges: Mutex<Vec<f64>>,
        histograms: Mutex<Vec<f64>>,
    }
    impl CounterFn for Observations {
        fn increment(&self, value: u64) {
            self.counter.fetch_add(value, Ordering::SeqCst);
        }
        fn absolute(&self, value: u64) {
            self.counter.fetch_max(value, Ordering::SeqCst);
        }
    }
    impl GaugeFn for Observations {
        fn increment(&self, value: f64) {
            self.gauges.lock().unwrap().push(value);
        }
        fn decrement(&self, value: f64) {
            self.gauges.lock().unwrap().push(-value);
        }
        fn set(&self, value: f64) {
            self.gauges.lock().unwrap().push(value);
        }
    }
    impl HistogramFn for Observations {
        fn record(&self, value: f64) {
            self.histograms.lock().unwrap().push(value);
        }
    }

    #[derive(Default)]
    struct Recording {
        descriptions: Mutex<Vec<(String, Option<Unit>)>>,
        keys: Mutex<Vec<Key>>,
        registrations: AtomicUsize,
        values: Arc<Observations>,
    }
    impl Recorder for Recording {
        fn describe_counter(&self, name: KeyName, unit: Option<Unit>, _: SharedString) {
            self.descriptions
                .lock()
                .unwrap()
                .push((name.as_str().into(), unit));
        }
        fn describe_gauge(&self, name: KeyName, unit: Option<Unit>, description: SharedString) {
            self.describe_counter(name, unit, description);
        }
        fn describe_histogram(&self, name: KeyName, unit: Option<Unit>, description: SharedString) {
            self.describe_counter(name, unit, description);
        }
        fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
            self.registrations.fetch_add(1, Ordering::SeqCst);
            self.keys.lock().unwrap().push(key.clone());
            Counter::from_arc(self.values.clone())
        }
        fn register_gauge(&self, key: &Key, _: &Metadata<'_>) -> Gauge {
            self.registrations.fetch_add(1, Ordering::SeqCst);
            self.keys.lock().unwrap().push(key.clone());
            Gauge::from_arc(self.values.clone())
        }
        fn register_histogram(&self, key: &Key, _: &Metadata<'_>) -> Histogram {
            self.registrations.fetch_add(1, Ordering::SeqCst);
            self.keys.lock().unwrap().push(key.clone());
            Histogram::from_arc(self.values.clone())
        }
    }

    fn scope(instance: &str) -> TelemetryScope {
        TelemetryScope::graph(instance, "graph").unwrap().component(
            ComponentId::try_new("064e948e-04b4-40d2-8dcd-32740a66e91f").unwrap(),
            ComponentRole::Transformer,
            ComponentSemanticKind::Query,
        )
    }

    fn scoped(
        context: &MetricRegistrationContext,
        generation: u64,
        metrics: bool,
        handles: usize,
    ) -> (
        Arc<super::super::LocalMetricBinding>,
        super::super::TelemetryLifetime,
        super::super::TelemetryDiagnostics,
    ) {
        use super::super::*;
        let lifetime = TelemetryLifetime::default();
        let diagnostics = TelemetryDiagnostics::default();
        let binding = LocalMetricBinding::new(
            Some(context.clone()),
            Arc::new(PluginTelemetryConfig {
                scope: scope("one"),
                generation,
                plugin_kind: "probe".into(),
                tracing: false,
                metrics,
                native_metrics_required: false,
                native_query_observations_required: false,
                trace_directives: "off".into(),
                log_directives: "off".into(),
                metric_exclusions: vec![],
                sampling: PluginRootSampling::Never,
                limits: TelemetryLimits {
                    metric_handles: handles,
                    ..Default::default()
                },
            }),
            lifetime.clone(),
            diagnostics.clone(),
        )
        .unwrap();
        (binding, lifetime, diagnostics)
    }

    #[derive(Debug, Default)]
    struct RecordingGuard {
        calls: AtomicUsize,
        names: Mutex<std::collections::BTreeMap<String, String>>,
    }

    impl MetricRegistrationGuard for RecordingGuard {
        fn validate_and_commit(&self, registration: &MetricRegistration) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let projected = registration.name().replace('.', "_");
            let mut names = self.names.lock().map_err(|_| TelemetryError::Poisoned)?;
            if names
                .get(&projected)
                .is_some_and(|existing| existing != registration.name())
            {
                return Err(TelemetryError::Conflict(registration.name().into()));
            }
            names.insert(projected, registration.name().into());
            Ok(())
        }
    }

    fn request(leaf: &str) -> PluginMetricRegistration {
        PluginMetricRegistration {
            leaf: leaf.into(),
            kind: MetricKind::Counter,
            unit: MetricUnit::One,
            description: "Received".into(),
            labels: vec![],
        }
    }

    #[test]
    fn guard_rejection_is_atomic() -> Result<()> {
        let recorder = Arc::new(Recording::default());
        let guard = Arc::new(RecordingGuard::default());
        let context = MetricRegistrationContext::new_with_guard(
            recorder.clone(),
            RegistrationLimits {
                definitions: 2,
                series: 2,
                ..Default::default()
            },
            guard.clone(),
        )?;
        let (binding, _, _) = scoped(&context, 1, true, 2);
        let _ = binding
            .emitter()
            .counter("received.total", MetricUnit::One, "Received")?;
        let state = context.registrations();
        let before_counts = state.counts()?;
        let backend_registration_calls = &recorder.registrations;
        let before_calls = backend_registration_calls.load(Ordering::SeqCst);
        let before_guard = guard.names.lock().unwrap().clone();
        let rejected = binding.register_handle(&request("received_total"));
        assert!(matches!(rejected, Err(TelemetryError::Conflict(_))));
        assert_eq!(state.counts()?, before_counts);
        assert_eq!(
            backend_registration_calls.load(Ordering::SeqCst),
            before_calls
        );
        assert_eq!(*guard.names.lock().unwrap(), before_guard);
        assert_eq!(recorder.descriptions.lock().unwrap().len(), 1);
        assert_eq!(binding.handle_count()?, 1);
        binding.register_handle(&request("other"))?.unwrap();
        assert_eq!(state.counts()?, (2, 2));
        assert_eq!(binding.handle_count()?, 2);
        Ok(())
    }

    #[test]
    fn capacity_failure_does_not_commit_guard() -> Result<()> {
        for (definitions, series, leaf, region, capacity) in [
            (1, 2, "other", "west", "instrument definitions"),
            (2, 1, "received", "east", "metric series"),
        ] {
            let recorder = Arc::new(Recording::default());
            let guard = Arc::new(RecordingGuard::default());
            let context = MetricRegistrationContext::new_with_guard(
                recorder.clone(),
                RegistrationLimits {
                    definitions,
                    series,
                    ..Default::default()
                },
                guard.clone(),
            )?;
            let (binding, _, _) = scoped(&context, 1, true, 4);
            let _ = binding
                .emitter()
                .with_labels([("region".into(), "west".into())])
                .counter("received", MetricUnit::One, "Received")?;
            let mut rejected = request(leaf);
            rejected.labels = vec![("region".into(), region.into())];
            assert_eq!(
                binding.register_handle(&rejected),
                Err(TelemetryError::Capacity(capacity))
            );
            assert_eq!(guard.calls.load(Ordering::SeqCst), 1);
            assert_eq!(guard.names.lock().unwrap().len(), 1);
            assert_eq!(context.registrations().counts()?, (1, 1));
            assert_eq!(recorder.registrations.load(Ordering::SeqCst), 1);
            assert_eq!(binding.handle_count()?, 1);
        }
        Ok(())
    }

    #[test]
    fn built_in_rejections_do_not_call_guard() -> Result<()> {
        let recorder = Arc::new(Recording::default());
        let guard = Arc::new(RecordingGuard::default());
        let context = MetricRegistrationContext::new_with_guard(
            recorder.clone(),
            RegistrationLimits {
                description_bytes: 8,
                ..Default::default()
            },
            guard.clone(),
        )?;
        let (first, _, _) = scoped(&context, 1, true, 1);
        first.register_handle(&request("received"))?.unwrap();
        let (second, _, _) = scoped(&context, 2, true, 1);
        let mut invalid = request("received");
        invalid.description = "Too long!".into();
        assert!(matches!(
            second.register_handle(&invalid),
            Err(TelemetryError::Invalid(_))
        ));
        let mut conflicting = request("received");
        conflicting.kind = MetricKind::Gauge;
        assert!(matches!(
            second.register_handle(&conflicting),
            Err(TelemetryError::Conflict(_))
        ));
        conflicting = request("received");
        conflicting.unit = MetricUnit::Seconds;
        assert!(matches!(
            second.register_handle(&conflicting),
            Err(TelemetryError::Conflict(_))
        ));
        conflicting = request("received");
        conflicting.labels = vec![("region".into(), "west".into())];
        assert!(matches!(
            second.register_handle(&conflicting),
            Err(TelemetryError::Conflict(_))
        ));
        assert_eq!(guard.calls.load(Ordering::SeqCst), 1);
        assert_eq!(guard.names.lock().unwrap().len(), 1);
        assert_eq!(context.registrations().counts()?, (1, 1));
        assert_eq!(recorder.registrations.load(Ordering::SeqCst), 1);
        assert_eq!(second.handle_count()?, 0);
        Ok(())
    }

    #[test]
    fn repeated_registration_is_idempotent() -> Result<()> {
        let recorder = Arc::new(Recording::default());
        let guard = Arc::new(RecordingGuard::default());
        let context = MetricRegistrationContext::new_with_guard(
            recorder.clone(),
            RegistrationLimits {
                definitions: 1,
                series: 1,
                ..Default::default()
            },
            guard.clone(),
        )?;
        let (first, _, _) = scoped(&context, 1, true, 1);
        let request = request("received");
        let handle = first.register_handle(&request)?.unwrap();
        assert_eq!(first.register_handle(&request)?, Some(handle));
        first
            .emitter()
            .counter("received", MetricUnit::One, "Received")?
            .increment(2);
        let (second, _, _) = scoped(&context.clone(), 2, true, 1);
        let replacement = second.register_handle(&request)?.unwrap();
        assert_ne!(replacement, handle);
        second
            .emitter()
            .counter("received", MetricUnit::One, "Received")?
            .increment(3);
        assert_eq!(guard.calls.load(Ordering::SeqCst), 2);
        assert_eq!(guard.names.lock().unwrap().len(), 1);
        assert_eq!(context.registrations().counts()?, (1, 1));
        assert_eq!(recorder.registrations.load(Ordering::SeqCst), 2);
        assert_eq!(recorder.values.counter.load(Ordering::SeqCst), 5);
        Ok(())
    }

    #[test]
    fn concurrent_guard_conflict_has_one_winner() -> Result<()> {
        let recorder = Arc::new(Recording::default());
        let guard = Arc::new(RecordingGuard::default());
        let context = MetricRegistrationContext::new_with_guard(
            recorder.clone(),
            Default::default(),
            guard.clone(),
        )?;
        let (first, _, _) = scoped(&context, 1, true, 1);
        let (second, _, _) = scoped(&context, 2, true, 1);
        let barrier = std::sync::Barrier::new(2);
        let concurrent_results = std::thread::scope(|threads| {
            let a = threads.spawn(|| {
                barrier.wait();
                first
                    .emitter()
                    .counter("received.total", MetricUnit::One, "Received")
                    .map(|_| ())
            });
            let b = threads.spawn(|| {
                barrier.wait();
                second
                    .register_handle(&request("received_total"))
                    .map(|_| ())
            });
            [a.join().unwrap(), b.join().unwrap()]
        });
        assert_eq!(concurrent_results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            concurrent_results
                .iter()
                .filter(|r| matches!(r, Err(TelemetryError::Conflict(_))))
                .count(),
            1
        );
        assert_eq!(context.registrations().counts()?, (1, 1));
        assert_eq!(guard.names.lock().unwrap().len(), 1);
        assert_eq!(recorder.registrations.load(Ordering::SeqCst), 1);
        assert_eq!(first.handle_count()? + second.handle_count()?, 1);
        Ok(())
    }

    #[test]
    fn unguarded_context_preserves_existing_behavior() -> Result<()> {
        let recorder = Arc::new(Recording::default());
        let context = MetricRegistrationContext::new(recorder.clone(), Default::default())?;
        let (binding, _, _) = scoped(&context, 1, true, 2);
        binding
            .emitter()
            .counter("received.total", MetricUnit::One, "Received")?
            .increment(2);
        binding
            .register_handle(&request("received_total"))?
            .unwrap();
        binding
            .emitter()
            .counter("received_total", MetricUnit::One, "Received")?
            .increment(3);
        assert_eq!(context.registrations().counts()?, (2, 2));
        assert_eq!(recorder.registrations.load(Ordering::SeqCst), 2);
        assert_eq!(recorder.values.counter.load(Ordering::SeqCst), 5);
        Ok(())
    }

    #[test]
    fn scoped_absolute_counts_accumulate_across_replacement_and_share_clones() {
        let recorder = Arc::new(Recording::default());
        let context = MetricRegistrationContext::new(recorder.clone(), Default::default()).unwrap();
        let (a, old, diagnostics) = scoped(&context, 1, true, 4);
        let counter = a
            .emitter()
            .counter("received", MetricUnit::One, "Received")
            .unwrap();
        counter.absolute(100);
        old.revoke(|| {}).unwrap();
        let (b, _, _) = scoped(&context, 2, true, 4);
        let replacement = b
            .emitter()
            .counter("received", MetricUnit::One, "Received")
            .unwrap();
        let registered_again = b
            .emitter()
            .counter("received", MetricUnit::One, "Received")
            .unwrap();
        replacement.absolute(10);
        registered_again.absolute(10);
        replacement.clone().absolute(8);
        registered_again.increment(5);
        replacement.absolute(15);
        replacement.absolute(20);
        counter.absolute(120);
        assert_eq!(recorder.values.counter.load(Ordering::SeqCst), 120);
        assert_eq!(recorder.registrations.load(Ordering::SeqCst), 2);
        assert_eq!(b.handle_count().unwrap(), 1);
        assert_eq!(diagnostics.snapshot().metric, 1);
        for key in recorder.keys.lock().unwrap().iter() {
            assert_eq!(key.name(), "drasi.plugin.probe.received");
            let labels: std::collections::BTreeMap<_, _> = key
                .labels()
                .map(|label| (label.key(), label.value()))
                .collect();
            assert_eq!(labels["plugin_kind"], "probe");
            assert_eq!(labels["instance_id"], "one");
            assert!(!labels.contains_key("generation"));
        }
    }

    #[test]
    fn scoped_concurrent_absolute_updates_and_revocation_are_linearized() {
        let recorder = Arc::new(Recording::default());
        let context = MetricRegistrationContext::new(recorder.clone(), Default::default()).unwrap();
        let (binding, lifetime, diagnostics) = scoped(&context, 1, true, 4);
        let counter = binding
            .emitter()
            .counter("received", MetricUnit::One, "Received")
            .unwrap();
        std::thread::scope(|threads| {
            for worker in 0..4 {
                let counter = counter.clone();
                threads.spawn(move || {
                    for value in (1..=100).rev() {
                        counter.absolute(value + worker * 100);
                    }
                });
            }
        });
        assert_eq!(recorder.values.counter.load(Ordering::SeqCst), 400);
        let barrier = Arc::new(std::sync::Barrier::new(5));
        std::thread::scope(|threads| {
            for _ in 0..4 {
                let counter = counter.clone();
                let barrier = barrier.clone();
                threads.spawn(move || {
                    for _ in 0..100 {
                        counter.increment(1);
                    }
                    barrier.wait();
                    barrier.wait();
                    for _ in 0..100 {
                        counter.increment(1);
                    }
                });
            }
            barrier.wait();
            lifetime.revoke(|| {}).unwrap();
            barrier.wait();
        });
        assert_eq!(recorder.values.counter.load(Ordering::SeqCst), 800);
        assert_eq!(diagnostics.snapshot().metric, 400);
    }

    #[test]
    fn scoped_gauges_reset_once_and_histograms_reject_invalid_values() {
        let recorder = Arc::new(Recording::default());
        let context = MetricRegistrationContext::new(recorder.clone(), Default::default()).unwrap();
        let (binding, lifetime, diagnostics) = scoped(&context, 1, true, 4);
        let emitter = binding.emitter();
        let gauge = emitter.gauge("queued", MetricUnit::One, "Queued").unwrap();
        gauge.set(7.0);
        let again = emitter.gauge("queued", MetricUnit::One, "Queued").unwrap();
        assert_eq!(*recorder.values.gauges.lock().unwrap(), vec![0.0, 7.0]);
        let histogram = emitter
            .histogram("decode.duration", MetricUnit::Seconds, "Decode")
            .unwrap();
        histogram.record(0.5);
        histogram.record(-0.5);
        histogram.record(f64::NAN);
        drop(emitter);
        drop(binding);
        lifetime.revoke(|| {}).unwrap();
        assert!(!lifetime.revoke(|| panic!("retired twice")).unwrap());
        again.set(2.0);
        gauge.increment(1.0);
        histogram.record(1.0);
        assert_eq!(*recorder.values.gauges.lock().unwrap(), vec![0.0, 7.0, 0.0]);
        assert_eq!(*recorder.values.histograms.lock().unwrap(), vec![0.5]);
        assert_eq!(diagnostics.snapshot().metric, 5);
        let (next, _, _) = scoped(&context, 2, true, 4);
        next.emitter()
            .gauge("queued", MetricUnit::One, "Queued")
            .unwrap()
            .set(3.0);
        gauge.set(100.0);
        assert_eq!(recorder.values.gauges.lock().unwrap().last(), Some(&3.0));
    }

    #[test]
    fn scoped_handle_limits_conflicts_disablements_and_foreign_updates_are_explicit() {
        use super::super::*;
        let recorder = Arc::new(Recording::default());
        let context = MetricRegistrationContext::new(recorder.clone(), Default::default()).unwrap();
        let (binding, lifetime, _) = scoped(&context, 1, true, 1);
        let request = PluginMetricRegistration {
            leaf: "received".into(),
            kind: MetricKind::Counter,
            unit: MetricUnit::One,
            description: "Received".into(),
            labels: vec![],
        };
        let handle = binding.register_handle(&request).unwrap().unwrap();
        assert_eq!(binding.register_handle(&request).unwrap(), Some(handle));
        assert!(matches!(
            binding
                .emitter()
                .gauge("received", MetricUnit::One, "Wrong type"),
            Err(TelemetryError::Conflict(_))
        ));
        assert!(matches!(
            binding
                .emitter()
                .counter("another", MetricUnit::One, "Other"),
            Err(TelemetryError::Capacity(_))
        ));
        assert_eq!(recorder.registrations.load(Ordering::SeqCst), 1);
        let (other, _, _) = scoped(&context, 2, true, 1);
        let foreign = other.register_handle(&request).unwrap().unwrap();
        assert_ne!(handle, foreign);
        assert!(other
            .update(PluginMetricUpdate::CounterIncrement { handle, value: 1 })
            .is_err());
        assert!(binding
            .update(PluginMetricUpdate::GaugeSet { handle, value: 1.0 })
            .is_err());
        binding
            .update(PluginMetricUpdate::CounterAbsolute {
                handle,
                value: u64::MAX,
            })
            .unwrap();
        assert!(binding
            .update(PluginMetricUpdate::CounterIncrement { handle, value: 1 })
            .is_err());
        assert_eq!(recorder.values.counter.load(Ordering::SeqCst), u64::MAX);
        lifetime.revoke(|| {}).unwrap();
        assert!(matches!(
            binding.register_handle(&request),
            Err(TelemetryError::Retired)
        ));
        let (disabled, _, _) = scoped(&context, 3, false, 1);
        disabled
            .emitter()
            .counter("INVALID", MetricUnit::One, "Unused")
            .unwrap()
            .increment(9);
        assert_eq!(disabled.handle_count().unwrap(), 0);
        assert_eq!(recorder.registrations.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn recording_is_typed_labelled_and_independent_of_sampling() {
        for traces in [false, true] {
            for sampling in [RootSampling::Always, RootSampling::Never] {
                let recorder = Arc::new(Recording::default());
                let context =
                    MetricRegistrationContext::new(recorder.clone(), RegistrationLimits::default())
                        .unwrap();
                let policy = TelemetrySettings::new(TelemetryProfile::Basic)
                    .with_tracing(traces)
                    .with_sampling(sampling)
                    .resolve()
                    .unwrap();
                context
                    .framework(
                        &policy,
                        &scope("one"),
                        "drasi.component.calls",
                        [("operation".into(), "transform".into())],
                    )
                    .unwrap()
                    .counter()
                    .unwrap()
                    .increment(3);
                context
                    .framework(&policy, &scope("one"), "drasi.component.up", [])
                    .unwrap()
                    .gauge()
                    .unwrap()
                    .set(1.0);
                context
                    .framework(
                        &policy,
                        &scope("one"),
                        "drasi.query.engine.duration",
                        [
                            ("query_id".into(), "query".into()),
                            ("phase".into(), "steady".into()),
                        ],
                    )
                    .unwrap()
                    .histogram()
                    .unwrap()
                    .record(0.25);
                assert_eq!(recorder.values.counter.load(Ordering::SeqCst), 3);
                assert_eq!(*recorder.values.gauges.lock().unwrap(), vec![1.0]);
                assert_eq!(*recorder.values.histograms.lock().unwrap(), vec![0.25]);
                assert_eq!(
                    recorder.descriptions.lock().unwrap()[2].1,
                    Some(Unit::Seconds)
                );
                for key in recorder.keys.lock().unwrap().iter() {
                    let labels: std::collections::BTreeMap<_, _> = key
                        .labels()
                        .map(|label| (label.key(), label.value()))
                        .collect();
                    assert_eq!(labels["instance_id"], "one");
                    assert_eq!(labels["graph_id"], "graph");
                    assert_eq!(labels["component_kind"], "query");
                    assert!(!labels.contains_key("generation"));
                }
            }
        }
    }

    #[test]
    fn disabled_and_excluded_registrations_do_no_recorder_work() {
        let recorder = Arc::new(Recording::default());
        let context =
            MetricRegistrationContext::new(recorder.clone(), RegistrationLimits::default())
                .unwrap();
        for policy in [
            TelemetryProfile::Off.resolve(),
            TelemetrySettings::new(TelemetryProfile::Debug)
                .exclude_metrics("component.")
                .resolve()
                .unwrap(),
        ] {
            context
                .framework(&policy, &scope("one"), "drasi.component.calls", [])
                .unwrap()
                .counter()
                .unwrap()
                .increment(100);
        }
        context
            .framework(
                &TelemetryProfile::Debug.resolve(),
                &scope("one"),
                "drasi.pipeline.end_to_end.duration",
                [],
            )
            .unwrap()
            .histogram()
            .unwrap()
            .record(1.0);
        assert_eq!(context.registrations().counts().unwrap(), (0, 0));
        assert_eq!(recorder.registrations.load(Ordering::SeqCst), 0);
        assert!(recorder.descriptions.lock().unwrap().is_empty());
        assert_eq!(recorder.values.counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn shared_context_rejects_conflicts_before_the_recorder() {
        let recorder = Arc::new(Recording::default());
        let context =
            MetricRegistrationContext::new(recorder.clone(), RegistrationLimits::default())
                .unwrap();
        for instance in ["one", "two"] {
            context
                .clone()
                .plugin(
                    &TelemetryProfile::Basic.resolve(),
                    &scope(instance),
                    "test",
                    PluginMetricDefinition {
                        leaf: "count",
                        kind: MetricKind::Counter,
                        unit: MetricUnit::One,
                        description: "Test",
                    },
                    [],
                )
                .unwrap()
                .counter()
                .unwrap()
                .increment(1);
        }
        assert!(matches!(
            context.plugin(
                &TelemetryProfile::Basic.resolve(),
                &scope("two"),
                "test",
                PluginMetricDefinition {
                    leaf: "count",
                    kind: MetricKind::Histogram,
                    unit: MetricUnit::Seconds,
                    description: "Test"
                },
                []
            ),
            Err(TelemetryError::Conflict(_))
        ));
        assert_eq!(recorder.registrations.load(Ordering::SeqCst), 2);
        assert_eq!(recorder.values.counter.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn previously_cached_noop_handles_are_not_rebound() {
        let noop =
            metrics::with_local_recorder(&metrics::NoopRecorder, || metrics::counter!("example"));
        let recorder = Recording::default();
        metrics::with_local_recorder(&recorder, || {
            noop.increment(10);
            metrics::counter!("example").increment(2);
        });
        assert_eq!(recorder.values.counter.load(Ordering::SeqCst), 2);
    }
}
