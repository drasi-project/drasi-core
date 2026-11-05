// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    facade::{MetricRegistrationContext, RegisteredMetric},
    lifetime::TelemetryRetirement,
    MetricKind, MetricRegistration, MetricUnit, PluginMetricHandle, PluginMetricRegistration,
    PluginMetricUpdate, PluginTelemetryConfig, Result, TelemetryDiagnostics, TelemetryError,
    TelemetryLifetime, TelemetryLoss, TelemetryRecord,
};
use metrics::{Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

pub trait ScopedMetricBackend: std::fmt::Debug + Send + Sync {
    fn register(&self, registration: &PluginMetricRegistration) -> Result<RegisteredMetric>;
}

/// Explicit, cloneable component scope; never uses the ambient global recorder.
#[derive(Debug, Clone)]
pub struct ScopedMetricEmitter {
    backend: Arc<dyn ScopedMetricBackend>,
    labels: Vec<(String, String)>,
}

impl ScopedMetricEmitter {
    pub fn disabled() -> Self {
        #[derive(Debug)]
        struct Disabled;
        impl ScopedMetricBackend for Disabled {
            fn register(&self, _: &PluginMetricRegistration) -> Result<RegisteredMetric> {
                Ok(RegisteredMetric::Disabled)
            }
        }
        Self::new(Arc::new(Disabled))
    }
    pub fn new(backend: Arc<dyn ScopedMetricBackend>) -> Self {
        Self {
            backend,
            labels: Vec::new(),
        }
    }

    pub fn with_labels(&self, labels: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            backend: self.backend.clone(),
            labels: labels.into_iter().collect(),
        }
    }

    fn register(
        &self,
        leaf: &str,
        kind: MetricKind,
        unit: MetricUnit,
        description: &str,
    ) -> Result<RegisteredMetric> {
        self.backend.register(&PluginMetricRegistration {
            leaf: leaf.into(),
            kind,
            unit,
            description: description.into(),
            labels: self.labels.clone(),
        })
    }

    pub fn counter(&self, leaf: &str, unit: MetricUnit, description: &str) -> Result<Counter> {
        self.register(leaf, MetricKind::Counter, unit, description)?
            .counter()
    }

    pub fn gauge(&self, leaf: &str, unit: MetricUnit, description: &str) -> Result<Gauge> {
        self.register(leaf, MetricKind::Gauge, unit, description)?
            .gauge()
    }

    pub fn histogram(&self, leaf: &str, unit: MetricUnit, description: &str) -> Result<Histogram> {
        self.register(leaf, MetricKind::Histogram, unit, description)?
            .histogram()
    }
}

type Series = (String, Vec<(String, String)>);

#[derive(Debug, Default)]
struct Registrations {
    series: BTreeMap<Series, (PluginMetricHandle, MetricKind, MetricUnit)>,
    handles: BTreeMap<PluginMetricHandle, Instrument>,
}

/// Host/static implementation. Native callbacks delegate to this same accounting.
#[derive(Debug)]
pub struct LocalMetricBinding {
    context: Option<MetricRegistrationContext>,
    config: Arc<PluginTelemetryConfig>,
    lifetime: TelemetryLifetime,
    diagnostics: TelemetryDiagnostics,
    registrations: Mutex<Registrations>,
}

impl LocalMetricBinding {
    pub fn new(
        context: Option<MetricRegistrationContext>,
        config: Arc<PluginTelemetryConfig>,
        lifetime: TelemetryLifetime,
        diagnostics: TelemetryDiagnostics,
    ) -> Result<Arc<Self>> {
        config.validate()?;
        if config.metrics && context.is_none() {
            return Err(TelemetryError::Invalid(
                "enabled scoped metrics require a host recorder".into(),
            ));
        }
        Ok(Arc::new(Self {
            context,
            config,
            lifetime,
            diagnostics,
            registrations: Mutex::new(Registrations::default()),
        }))
    }

    pub fn emitter(self: &Arc<Self>) -> ScopedMetricEmitter {
        ScopedMetricEmitter::new(self.clone())
    }

    pub fn register_handle(
        &self,
        request: &PluginMetricRegistration,
    ) -> Result<Option<PluginMetricHandle>> {
        if !self.config.allows_metric(&request.leaf) {
            return Ok(None);
        }
        self.lifetime.with_active(|| {
            request.validate()?;
            let registration = MetricRegistration::plugin(
                &self.config.scope,
                &self.config.plugin_kind,
                &request.leaf,
                request.kind,
                request.unit,
                request.description.clone(),
                request.labels.clone(),
            )?;
            let key = (
                registration.name().to_owned(),
                registration.labels().to_vec(),
            );
            let mut registrations = self
                .registrations
                .lock()
                .map_err(|_| TelemetryError::Poisoned)?;
            if let Some((handle, kind, unit)) = registrations.series.get(&key) {
                if *kind != request.kind || *unit != request.unit {
                    return Err(TelemetryError::Conflict(registration.name().into()));
                }
                return Ok(Some(*handle));
            }
            if registrations.handles.len() >= self.config.limits.metric_handles {
                return Err(TelemetryError::Capacity("scoped metric handles"));
            }
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let token = NEXT
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    value.checked_add(1)
                })
                .map_err(|_| TelemetryError::Capacity("metric handle tokens"))?;
            let context = self
                .context
                .as_ref()
                .ok_or_else(|| TelemetryError::Invalid("scoped recorder is unavailable".into()))?;
            let metric = context.register(registration)?;
            let common = MetricOwner {
                lifetime: self.lifetime.clone(),
                diagnostics: self.diagnostics.clone(),
            };
            let instrument = match metric {
                RegisteredMetric::Counter(raw) => Instrument::Counter(Arc::new(ScopedCounter {
                    raw,
                    local: Mutex::new(0),
                    common,
                })),
                RegisteredMetric::Gauge(raw) => {
                    raw.set(0.0);
                    let gauge = Arc::new(ScopedGauge { raw, common });
                    let listener: Arc<dyn TelemetryRetirement> = gauge.clone();
                    self.lifetime.on_retire(Arc::downgrade(&listener))?;
                    Instrument::Gauge(gauge)
                }
                RegisteredMetric::Histogram(raw) => {
                    Instrument::Histogram(Arc::new(ScopedHistogram {
                        raw,
                        unit: request.unit,
                        common,
                    }))
                }
                RegisteredMetric::Disabled => {
                    return Err(TelemetryError::Invalid(
                        "enabled registration returned no instrument".into(),
                    ))
                }
            };
            let handle = PluginMetricHandle(token);
            registrations
                .series
                .insert(key, (handle, request.kind, request.unit));
            registrations.handles.insert(handle, instrument);
            Ok(Some(handle))
        })
    }

    pub fn update(&self, update: PluginMetricUpdate) -> Result<()> {
        update.validate()?;
        let instrument = self
            .registrations
            .lock()
            .map_err(|_| TelemetryError::Poisoned)?
            .handles
            .get(&update.handle())
            .cloned()
            .ok_or_else(|| {
                TelemetryError::Invalid("metric handle does not belong to this binding".into())
            })?;
        instrument.update(update)
    }

    pub fn handle_count(&self) -> Result<usize> {
        Ok(self
            .registrations
            .lock()
            .map_err(|_| TelemetryError::Poisoned)?
            .handles
            .len())
    }
}

impl ScopedMetricBackend for LocalMetricBinding {
    fn register(&self, registration: &PluginMetricRegistration) -> Result<RegisteredMetric> {
        let Some(handle) = self.register_handle(registration)? else {
            return Ok(RegisteredMetric::Disabled);
        };
        self.registrations
            .lock()
            .map_err(|_| TelemetryError::Poisoned)?
            .handles
            .get(&handle)
            .map(Instrument::facade)
            .ok_or_else(|| {
                TelemetryError::Invalid("registered metric handle is unavailable".into())
            })
    }
}

#[derive(Debug, Clone)]
enum Instrument {
    Counter(Arc<ScopedCounter>),
    Gauge(Arc<ScopedGauge>),
    Histogram(Arc<ScopedHistogram>),
}

impl Instrument {
    fn facade(&self) -> RegisteredMetric {
        match self {
            Self::Counter(counter) => RegisteredMetric::Counter(Counter::from_arc(counter.clone())),
            Self::Gauge(gauge) => RegisteredMetric::Gauge(Gauge::from_arc(gauge.clone())),
            Self::Histogram(histogram) => {
                RegisteredMetric::Histogram(Histogram::from_arc(histogram.clone()))
            }
        }
    }

    fn update(&self, update: PluginMetricUpdate) -> Result<()> {
        match (self, update) {
            (Self::Counter(counter), PluginMetricUpdate::CounterIncrement { value, .. }) => {
                counter.update(value, false)
            }
            (Self::Counter(counter), PluginMetricUpdate::CounterAbsolute { value, .. }) => {
                counter.update(value, true)
            }
            (Self::Gauge(gauge), PluginMetricUpdate::GaugeSet { value, .. }) => {
                gauge.update(value, GaugeOperation::Set)
            }
            (Self::Gauge(gauge), PluginMetricUpdate::GaugeIncrement { value, .. }) => {
                gauge.update(value, GaugeOperation::Increment)
            }
            (Self::Gauge(gauge), PluginMetricUpdate::GaugeDecrement { value, .. }) => {
                gauge.update(value, GaugeOperation::Decrement)
            }
            (Self::Histogram(histogram), PluginMetricUpdate::HistogramRecord { value, .. }) => {
                histogram.update(value)
            }
            _ => Err(TelemetryError::Invalid(
                "metric operation does not match instrument type".into(),
            )),
        }
    }
}

#[derive(Debug)]
struct MetricOwner {
    lifetime: TelemetryLifetime,
    diagnostics: TelemetryDiagnostics,
}

impl MetricOwner {
    fn observe(&self, result: Result<()>) {
        if result.is_err() {
            self.diagnostics.record(TelemetryLoss::Metric);
        }
    }
}

#[derive(Debug)]
struct ScopedCounter {
    raw: Counter,
    local: Mutex<u64>,
    common: MetricOwner,
}

impl ScopedCounter {
    fn update(&self, value: u64, absolute: bool) -> Result<()> {
        self.common.lifetime.with_active(|| {
            let mut local = self.local.lock().map_err(|_| TelemetryError::Poisoned)?;
            let delta = if absolute {
                value.saturating_sub(*local)
            } else {
                value
            };
            let next = local
                .checked_add(delta)
                .ok_or_else(|| TelemetryError::Invalid("scoped counter overflow".into()))?;
            if delta != 0 {
                self.raw.increment(delta);
            }
            *local = next;
            Ok(())
        })
    }
}

impl CounterFn for ScopedCounter {
    fn increment(&self, value: u64) {
        self.common.observe(self.update(value, false));
    }
    fn absolute(&self, value: u64) {
        self.common.observe(self.update(value, true));
    }
}

enum GaugeOperation {
    Set,
    Increment,
    Decrement,
}

#[derive(Debug)]
struct ScopedGauge {
    raw: Gauge,
    common: MetricOwner,
}

impl ScopedGauge {
    fn update(&self, value: f64, operation: GaugeOperation) -> Result<()> {
        self.common.lifetime.with_active(|| {
            if !value.is_finite() {
                return Err(TelemetryError::Invalid("non-finite gauge update".into()));
            }
            match operation {
                GaugeOperation::Set => self.raw.set(value),
                GaugeOperation::Increment => self.raw.increment(value),
                GaugeOperation::Decrement => self.raw.decrement(value),
            }
            Ok(())
        })
    }
}
impl GaugeFn for ScopedGauge {
    fn set(&self, value: f64) {
        self.common.observe(self.update(value, GaugeOperation::Set));
    }
    fn increment(&self, value: f64) {
        self.common
            .observe(self.update(value, GaugeOperation::Increment));
    }
    fn decrement(&self, value: f64) {
        self.common
            .observe(self.update(value, GaugeOperation::Decrement));
    }
}
impl TelemetryRetirement for ScopedGauge {
    fn retire(&self) {
        self.raw.set(0.0);
    }
}

#[derive(Debug)]
struct ScopedHistogram {
    raw: Histogram,
    unit: MetricUnit,
    common: MetricOwner,
}
impl ScopedHistogram {
    fn update(&self, value: f64) -> Result<()> {
        self.common.lifetime.with_active(|| {
            if !value.is_finite() || (self.unit == MetricUnit::Seconds && value < 0.0) {
                return Err(TelemetryError::Invalid(
                    "invalid histogram observation".into(),
                ));
            }
            self.raw.record(value);
            Ok(())
        })
    }
}
impl HistogramFn for ScopedHistogram {
    fn record(&self, value: f64) {
        self.common.observe(self.update(value));
    }
}
