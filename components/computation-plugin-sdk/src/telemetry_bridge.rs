// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{
    abi,
    telemetry::*,
    transport::{self, Failure},
    wire,
};
use metrics::{Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn};
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, OnceLock,
    },
};
use tracing_subscriber::filter::Targets;

struct HostLease(abi::telemetry::HostTelemetryV2);
// The negotiated callback contract explicitly permits concurrent calls/releases.
unsafe impl Send for HostLease {}
unsafe impl Sync for HostLease {}
impl std::fmt::Debug for HostLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HostTelemetryLease")
    }
}
impl Drop for HostLease {
    fn drop(&mut self) {
        unsafe { (self.0.release.expect("validated"))(self.0.context) };
    }
}

static BINDINGS: AtomicUsize = AtomicUsize::new(0);
struct BindingSlot;
impl Drop for BindingSlot {
    fn drop(&mut self) {
        BINDINGS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
struct ClientState {
    host: HostLease,
    query: Option<Arc<crate::query_transport::QueryClient>>,
    config: Arc<PluginTelemetryConfig>,
    traces: Targets,
    logs: Targets,
    diagnostics: TelemetryDiagnostics,
    _slot: BindingSlot,
}
impl std::fmt::Debug for BindingSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("NativeTelemetryBindingSlot")
    }
}

#[derive(Debug, Clone)]
struct Client(Arc<ClientState>);

impl Client {
    unsafe fn new(
        host: *const abi::telemetry::HostTelemetryV2,
        bytes: abi::BorrowedBytes,
        query: Option<Arc<crate::query_transport::QueryClient>>,
    ) -> anyhow::Result<(Self, ExecutionContext)> {
        let host = *unsafe { transport::checked_table(host)? };
        anyhow::ensure!(
            host.version == abi::telemetry::VERSION_V2
                && host.capabilities & !abi::telemetry::ALL == 0
                && host.capabilities & abi::telemetry::CONTEXT != 0
                && !host.context.is_null()
                && host.retain.is_some()
                && host.release.is_some()
                && (host.capabilities & abi::telemetry::SPANS == 0
                    || (host.submit_span.is_some() && host.sample_root.is_some()))
                && (host.capabilities & abi::telemetry::LOGS == 0 || host.submit_log.is_some())
                && (host.capabilities & abi::telemetry::METRICS == 0
                    || (host.register_metric.is_some() && host.update_metric.is_some())),
            "incompatible host telemetry callbacks"
        );
        let frame: PluginTelemetryScope = decode_telemetry(
            unsafe { transport::borrowed_bytes(bytes, MAX_TELEMETRY_RECORD_BYTES)? },
            MAX_TELEMETRY_RECORD_BYTES,
        )?;
        let config = frame.config;
        anyhow::ensure!(
            !config.native_query_observations_required || query.is_some(),
            "required native query observation callbacks are unavailable"
        );
        let context = frame.context.map(ExecutionContext::new).unwrap_or_default();
        anyhow::ensure!(
            !config.tracing || host.capabilities & abi::telemetry::SPANS != 0,
            "native spans requested without a host collector"
        );
        anyhow::ensure!(
            !config.metrics || host.capabilities & abi::telemetry::METRICS != 0,
            "native metrics requested without callbacks"
        );
        let traces = config
            .trace_directives
            .parse::<Targets>()
            .map_err(|error| {
                anyhow::anyhow!("native trace filters require target/level directives: {error}")
            })?;
        let logs = config.log_directives.parse::<Targets>().map_err(|error| {
            anyhow::anyhow!("native log filters require target/level directives: {error}")
        })?;
        BINDINGS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                (value < MAX_NATIVE_BINDINGS).then_some(value + 1)
            })
            .map_err(|_| anyhow::anyhow!("native telemetry binding capacity exhausted"))?;
        let slot = BindingSlot;
        unsafe { (host.retain.expect("validated"))(host.context) };
        Ok((
            Self(Arc::new(ClientState {
                host: HostLease(host),
                query,
                config: Arc::new(config),
                traces,
                logs,
                diagnostics: crate::telemetry_capture::diagnostics().clone(),
                _slot: slot,
            })),
            context,
        ))
    }

    fn encode<T: TelemetryRecord>(&self, record: &T, reason: TelemetryLoss) -> Result<Vec<u8>> {
        encode_telemetry(record, self.0.config.limits.record_bytes)
            .inspect_err(|_| self.0.diagnostics.record(reason))
    }

    fn submission(&self, result: u32) -> TelemetrySubmission {
        TelemetrySubmission::try_from(result).unwrap_or_else(|_| {
            self.0.diagnostics.record(TelemetryLoss::Capture);
            TelemetrySubmission::Invalid
        })
    }
}

impl PluginTelemetryBackend for Client {
    fn supports_query_observations(&self) -> bool {
        self.0.query.is_some()
    }
    fn begin_query_observation(
        &self,
        request: &QueryObservationRequest,
    ) -> Result<Option<Box<dyn QueryObservation>>> {
        match &self.0.query {
            Some(query) => query
                .begin(request, self.0.config.limits.record_bytes)
                .inspect_err(|_| self.0.diagnostics.record(TelemetryLoss::Invalid)),
            None => Ok(None),
        }
    }
    fn query_observation_event(&self, event: QueryObservationEvent) -> Result<()> {
        match &self.0.query {
            Some(query) => query
                .event(event, self.0.config.limits.record_bytes)
                .inspect_err(|_| self.0.diagnostics.record(TelemetryLoss::Invalid)),
            None => Ok(()),
        }
    }
    fn config(&self) -> &PluginTelemetryConfig {
        &self.0.config
    }
    fn diagnostics(&self) -> &TelemetryDiagnostics {
        &self.0.diagnostics
    }
    fn for_plugin(&self, kind: &str) -> Result<PluginTelemetry> {
        if kind != self.0.config.plugin_kind {
            return Err(TelemetryError::Invalid(
                "native telemetry cannot override its plugin kind".into(),
            ));
        }
        Ok(PluginTelemetry::new(Arc::new(self.clone())))
    }
    fn register_metric(&self, registration: &PluginMetricRegistration) -> Result<RegisteredMetric> {
        let Some(handle) = self.register_metric_handle(registration)? else {
            return Ok(RegisteredMetric::Disabled);
        };
        let metric = Arc::new(ForeignMetric {
            client: self.clone(),
            handle,
        });
        Ok(match registration.kind {
            MetricKind::Counter => RegisteredMetric::Counter(Counter::from_arc(metric)),
            MetricKind::Gauge => RegisteredMetric::Gauge(Gauge::from_arc(metric)),
            MetricKind::Histogram => RegisteredMetric::Histogram(Histogram::from_arc(metric)),
        })
    }
    fn register_metric_handle(
        &self,
        registration: &PluginMetricRegistration,
    ) -> Result<Option<PluginMetricHandle>> {
        if !self.0.config.allows_metric(&registration.leaf) {
            return Ok(None);
        }
        let bytes = self.encode(registration, TelemetryLoss::Metric)?;
        let callback = self
            .0
            .host
            .0
            .register_metric
            .ok_or(TelemetryError::Unsupported("native metric registration"))?;
        let reply = unsafe {
            transport::take_reply(callback(
                self.0.host.0.context,
                abi::BorrowedBytes::new(&bytes),
            ))
        }
        .map_err(|error| TelemetryError::Invalid(error.to_string()))?;
        if reply.len() > 32 {
            return Err(TelemetryError::Invalid(
                "oversized native metric handle".into(),
            ));
        }
        let handle: Option<PluginMetricHandle> =
            wire::decode(&reply).map_err(|error| TelemetryError::Invalid(error.to_string()))?;
        if handle.is_some_and(|handle| handle.0 == 0) {
            return Err(TelemetryError::Invalid("zero native metric handle".into()));
        }
        Ok(handle)
    }
    fn update_metric(&self, update: PluginMetricUpdate) -> Result<()> {
        let bytes = self.encode(&update, TelemetryLoss::Metric)?;
        let callback = self
            .0
            .host
            .0
            .update_metric
            .ok_or(TelemetryError::Unsupported("native metric update"))?;
        unsafe {
            transport::take_status(callback(
                self.0.host.0.context,
                abi::BorrowedBytes::new(&bytes),
            ))
        }
        .map_err(|error| TelemetryError::Invalid(error.to_string()))
    }
    fn has_span_collector(&self) -> bool {
        self.0.host.0.capabilities & abi::telemetry::SPANS != 0
    }
    fn enabled(&self, target: &str, level: TelemetryLogLevel, span: bool) -> bool {
        let level = crate::telemetry_capture::tracing_level(level);
        if span {
            self.0.config.tracing && self.0.traces.would_enable(target, &level)
        } else {
            self.0.host.0.capabilities & abi::telemetry::LOGS != 0
                && self.0.logs.would_enable(target, &level)
        }
    }
    fn submit_encoded_span(&self, bytes: &[u8]) -> TelemetrySubmission {
        if !self.0.config.tracing {
            return TelemetrySubmission::Accepted;
        }
        if bytes.len() > self.0.config.limits.record_bytes {
            self.0.diagnostics.record(TelemetryLoss::Capture);
            return TelemetrySubmission::Invalid;
        }
        match self.0.host.0.submit_span {
            Some(callback) => self.submission(unsafe {
                callback(self.0.host.0.context, abi::BorrowedBytes::new(bytes))
            }),
            None => {
                self.0.diagnostics.record(TelemetryLoss::Capture);
                TelemetrySubmission::Invalid
            }
        }
    }
    fn submit_span(&self, span: &CompletedSpan) -> TelemetrySubmission {
        match self.encode(span, TelemetryLoss::Capture) {
            Ok(bytes) => self.submit_encoded_span(&bytes),
            Err(_) => TelemetrySubmission::Invalid,
        }
    }
    fn submit_log(&self, record: &TelemetryLogRecord) -> TelemetrySubmission {
        if !self.enabled(&record.target, record.level, false) {
            return TelemetrySubmission::Accepted;
        }
        match (
            self.0.host.0.submit_log,
            self.encode(record, TelemetryLoss::Capture),
        ) {
            (Some(callback), Ok(bytes)) => self.submission(unsafe {
                callback(self.0.host.0.context, abi::BorrowedBytes::new(&bytes))
            }),
            _ => TelemetrySubmission::Invalid,
        }
    }
    fn sample_root(&self, request: &RootSpanRequest) -> Result<RootSpanDecision> {
        let bytes = self.encode(request, TelemetryLoss::Capture)?;
        let callback = self
            .0
            .host
            .0
            .sample_root
            .ok_or(TelemetryError::Unsupported("host root sampling"))?;
        let reply = unsafe {
            transport::take_reply(callback(
                self.0.host.0.context,
                abi::BorrowedBytes::new(&bytes),
            ))
        }
        .map_err(|error| TelemetryError::Invalid(error.to_string()))?;
        decode_telemetry(&reply, self.0.config.limits.record_bytes)
    }
}

struct ForeignMetric {
    client: Client,
    handle: PluginMetricHandle,
}
impl ForeignMetric {
    fn update(&self, update: PluginMetricUpdate) {
        // Encoding and host callbacks report rejection before returning. Logging
        // again here would itself be unattributed for a cloned worker handle.
        let _reported = self.client.update_metric(update);
    }
}
impl CounterFn for ForeignMetric {
    fn increment(&self, value: u64) {
        self.update(PluginMetricUpdate::CounterIncrement {
            handle: self.handle,
            value,
        });
    }
    fn absolute(&self, value: u64) {
        self.update(PluginMetricUpdate::CounterAbsolute {
            handle: self.handle,
            value,
        });
    }
}
impl GaugeFn for ForeignMetric {
    fn set(&self, value: f64) {
        self.update(PluginMetricUpdate::GaugeSet {
            handle: self.handle,
            value,
        });
    }
    fn increment(&self, value: f64) {
        self.update(PluginMetricUpdate::GaugeIncrement {
            handle: self.handle,
            value,
        });
    }
    fn decrement(&self, value: f64) {
        self.update(PluginMetricUpdate::GaugeDecrement {
            handle: self.handle,
            value,
        });
    }
}
impl HistogramFn for ForeignMetric {
    fn record(&self, value: f64) {
        self.update(PluginMetricUpdate::HistogramRecord {
            handle: self.handle,
            value,
        });
    }
}

/// # Safety
/// Tables/buffers and the one-call callback state obey the telemetry V2 contract.
pub unsafe extern "C" fn with_scope(
    host: *const abi::telemetry::HostTelemetryV2,
    config: abi::BorrowedBytes,
    call: abi::telemetry::ScopedCall,
    state: *mut c_void,
) -> abi::Status {
    transport::status_boundary(|| {
        let (client, context) =
            unsafe { Client::new(host, config, None) }.map_err(Failure::from)?;
        let _dispatch = tracing::dispatcher::set_default(crate::telemetry_capture::dispatch());
        let binding = PluginTelemetry::new(Arc::new(client));
        let scope = ExecutionScope::new(context)
            .with_component(binding.config().scope.clone(), binding.config().generation)
            .with_plugin_telemetry(Some(binding))
            .with_adapter(Some(Arc::new(
                crate::telemetry_capture::NativeContextAdapter,
            )));
        scope.in_scope(|| unsafe { transport::take_status(call(state)) })
    })
}

/// # Safety
/// Both callback tables and the synchronous borrowing callback obey telemetry V4.
pub unsafe extern "C" fn with_query_scope(
    host: *const abi::telemetry::HostTelemetryV2,
    query: *const abi::telemetry::HostQueryTelemetryV4,
    config: abi::BorrowedBytes,
    call: abi::telemetry::ScopedCall,
    state: *mut c_void,
) -> abi::Status {
    transport::status_boundary(|| {
        let query =
            unsafe { crate::query_transport::QueryClient::new(query) }.map_err(Failure::from)?;
        let (client, context) =
            unsafe { Client::new(host, config, Some(query)) }.map_err(Failure::from)?;
        let _dispatch = tracing::dispatcher::set_default(crate::telemetry_capture::dispatch());
        let binding = PluginTelemetry::new(Arc::new(client));
        let scope = ExecutionScope::new(context)
            .with_component(binding.config().scope.clone(), binding.config().generation)
            .with_plugin_telemetry(Some(binding))
            .with_adapter(Some(Arc::new(
                crate::telemetry_capture::NativeContextAdapter,
            )));
        scope.in_scope(|| unsafe { transport::take_status(call(state)) })
    })
}

pub fn initialize_native_logging() -> anyhow::Result<()> {
    static INITIALIZED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    INITIALIZED
        .get_or_init(|| {
            log::set_logger(&crate::telemetry_capture::LOGGER)
                .map_err(|error| error.to_string())?;
            log::set_max_level(log::LevelFilter::Trace);
            metrics::set_global_recorder(UnscopedRecorder).map_err(|error| error.to_string())?;
            Ok(())
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

pub fn native_telemetry_losses() -> TelemetryLossSnapshot {
    crate::telemetry_capture::loss_snapshot()
}

struct UnscopedRecorder;
impl metrics::Recorder for UnscopedRecorder {
    fn describe_counter(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_gauge(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_histogram(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn register_counter(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> Counter {
        crate::telemetry_capture::unattributed();
        Counter::noop()
    }
    fn register_gauge(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> Gauge {
        crate::telemetry_capture::unattributed();
        Gauge::noop()
    }
    fn register_histogram(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> Histogram {
        crate::telemetry_capture::unattributed();
        Histogram::noop()
    }
}
