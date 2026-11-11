// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_plugin_abi::telemetry::{METRICS, SPANS};
use drasi_computation_plugin_abi::{self as abi, telemetry::*};
use drasi_computation_plugin_sdk::{
    transport::{self, Failure},
    wire,
};
use drasi_lib::observability::*;
#[cfg(test)]
#[path = "query_telemetry_tests.rs"]
mod query_tests;
use std::{
    collections::BTreeMap,
    ffi::c_void,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

static NEXT_QUERY_BINDING: AtomicU64 = AtomicU64::new(1);
static OPEN_QUERY_OBSERVATIONS: AtomicUsize = AtomicUsize::new(0);

struct QuerySlot(Arc<AtomicUsize>);
impl Drop for QuerySlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
        OPEN_QUERY_OBSERVATIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct QueryEntry {
    observation: Mutex<Option<Box<dyn QueryObservation>>>,
    diagnostics: TelemetryDiagnostics,
    _slot: QuerySlot,
}

impl Drop for QueryEntry {
    fn drop(&mut self) {
        let observation = self
            .observation
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(mut observation) = observation.take() {
            if let Err(error) = observation.finish(QueryObservationResult {
                outcome: QueryObservationOutcome::Cancelled,
                result_count: None,
            }) {
                self.diagnostics.record(TelemetryLoss::Invalid);
                log::error!("Cancelling abandoned native query observation: {error}");
            }
        }
    }
}

#[derive(Default)]
struct Queries {
    next: u64,
    entries: BTreeMap<u64, Arc<QueryEntry>>,
}

struct BindingState {
    backend: PluginTelemetry,
    lifetime: TelemetryLifetime,
    query_owner: [u8; 16],
    queries: Mutex<Queries>,
    query_count: Arc<AtomicUsize>,
    processing_context: Mutex<Option<ExecutionContext>>,
}

pub(super) struct NativeTelemetryBinding {
    state: Arc<BindingState>,
    extension: PluginTelemetryV2,
    config: PluginTelemetryConfig,
    capabilities: u32,
    query: Option<PluginTelemetryV4>,
}

impl Drop for NativeTelemetryBinding {
    fn drop(&mut self) {
        if let Err(error) = self.state.lifetime.revoke(|| {}) {
            log::error!("Revoking native telemetry callbacks: {error}");
        }
        match self.state.queries.lock() {
            Ok(mut queries) => {
                let entries = std::mem::take(&mut queries.entries);
                drop(queries);
                drop(entries);
            }
            Err(error) => {
                self.state
                    .backend
                    .diagnostics()
                    .record(TelemetryLoss::Invalid);
                let mut queries = error.into_inner();
                let entries = std::mem::take(&mut queries.entries);
                drop(queries);
                drop(entries);
            }
        }
        if let Err(error) = self.state.backend.retire() {
            log::error!("Retiring native telemetry instruments: {error}");
        }
    }
}

impl NativeTelemetryBinding {
    pub(super) fn capture(
        extension: Option<PluginTelemetryV2>,
        kind: &str,
    ) -> anyhow::Result<Option<Self>> {
        Self::capture_with_query(extension, None, kind, false)
    }

    pub(super) fn capture_query(
        extension: Option<PluginTelemetryV2>,
        query: Option<PluginTelemetryV4>,
        kind: &str,
    ) -> anyhow::Result<Option<Self>> {
        Self::capture_with_query(extension, query, kind, true)
    }

    fn capture_with_query(
        extension: Option<PluginTelemetryV2>,
        query: Option<PluginTelemetryV4>,
        kind: &str,
        inspect_query: bool,
    ) -> anyhow::Result<Option<Self>> {
        let Some(parent) = ExecutionScope::current_plugin_telemetry() else {
            return Ok(None);
        };
        let kind: String = kind
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                    ch.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let backend = parent.for_plugin(&kind)?;
        let is_query = inspect_query
            && backend.config().scope.semantic_kind()
                == Some(drasi_lib::computation::v1::ComponentSemanticKind::Query);
        if is_query && (query.is_none() || extension.is_none()) {
            if backend.config().native_query_observations_required {
                return Err(
                    TelemetryError::Unsupported("required native query observations").into(),
                );
            }
            if backend.config().tracing || backend.config().metrics {
                backend.diagnostics().record(TelemetryLoss::Capture);
                log::warn!("Native query observations unavailable for {kind}; optional telemetry V4 and a host query backend are required");
            }
        }
        let query = query.filter(|_| is_query && backend.supports_query_observations());
        let Some(extension) = extension else {
            if backend.config().native_metrics_required {
                return Err(TelemetryError::Unsupported("required native metric callbacks").into());
            }
            if backend.config().tracing
                || backend.config().metrics
                || backend.config().log_directives != "off"
            {
                log::warn!("Native plugin {kind} has no shared telemetry extension; internal spans/logs/metrics are unavailable");
            }
            return Ok(None);
        };
        if backend.config().native_metrics_required && extension.capabilities & METRICS == 0 {
            return Err(TelemetryError::Unsupported("required native metric callbacks").into());
        }
        let mut config = backend.config().clone();
        config.native_query_observations_required &= query.is_some();
        let capabilities = extension.capabilities
            & (CONTEXT
                | LOGS
                | METRICS
                | if backend.has_span_collector() {
                    SPANS
                } else {
                    0
                });
        if config.tracing && capabilities & SPANS == 0 {
            log::warn!("Native internal span collection unavailable for {kind}; supply a host span collector and a span-capable plugin");
            config.tracing = false;
        }
        if config.metrics && capabilities & METRICS == 0 {
            log::warn!("Native metric collection unavailable for {kind}");
            config.metrics = false;
        }
        if config.log_directives != "off" && capabilities & LOGS == 0 {
            log::warn!("Native log collection unavailable for {kind}");
            config.log_directives = "off".into();
        }
        config.validate()?;
        let owner = NEXT_QUERY_BINDING
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| TelemetryError::Capacity("native query binding identities"))?;
        let mut query_owner = [0; 16];
        query_owner[..8].copy_from_slice(&owner.to_le_bytes());
        query_owner[8..].copy_from_slice(&config.generation.to_le_bytes());
        Ok(Some(Self {
            state: Arc::new(BindingState {
                backend,
                lifetime: Default::default(),
                query_owner,
                queries: Default::default(),
                query_count: Default::default(),
                processing_context: Default::default(),
            }),
            extension,
            config,
            capabilities,
            query,
        }))
    }

    pub(super) fn query_observations_active(&self) -> bool {
        self.query.is_some()
    }

    pub(super) fn reset_processing_context(&self) -> anyhow::Result<()> {
        *self
            .state
            .processing_context
            .lock()
            .map_err(|_| TelemetryError::Poisoned)? = None;
        Ok(())
    }

    pub(super) fn processing_context(&self) -> Option<ExecutionContext> {
        self.query.map(|_| {
            self.state
                .processing_context
                .lock()
                .ok()
                .and_then(|context| context.clone())
                .unwrap_or_default()
        })
    }

    fn query_table(&self) -> HostQueryTelemetryV4 {
        HostQueryTelemetryV4 {
            header: abi::Header::new::<HostQueryTelemetryV4>(),
            version: VERSION_V4,
            reserved: 0,
            context: Arc::as_ptr(&self.state).cast_mut().cast(),
            retain: Some(retain),
            release: Some(release),
            begin: Some(begin_query),
            in_scope: Some(query_scope),
            finish: Some(finish_query),
            event: Some(query_event),
        }
    }

    fn table(&self) -> HostTelemetryV2 {
        HostTelemetryV2 {
            header: abi::Header::new::<HostTelemetryV2>(),
            version: VERSION_V2,
            capabilities: self.capabilities,
            context: Arc::as_ptr(&self.state).cast_mut().cast(),
            retain: Some(retain),
            release: Some(release),
            submit_span: Some(submit_span),
            submit_log: Some(submit_log),
            register_metric: Some(register_metric),
            update_metric: Some(update_metric),
            sample_root: Some(sample_root),
        }
    }

    pub(super) fn scope<T, F: FnOnce() -> anyhow::Result<T>>(&self, work: F) -> anyhow::Result<T> {
        struct Call<F, T> {
            work: Option<F>,
            result: Option<anyhow::Result<T>>,
        }
        unsafe extern "C" fn invoke<F: FnOnce() -> anyhow::Result<T>, T>(
            state: *mut c_void,
        ) -> abi::Status {
            transport::status_boundary(|| {
                let state = unsafe { &mut *state.cast::<Call<F, T>>() };
                let work = state.work.take().ok_or_else(|| {
                    Failure::protocol("native telemetry construction callback invoked twice")
                })?;
                let result = work();
                let failed = result.is_err();
                state.result = Some(result);
                if failed {
                    return Err(Failure::failed("host retains scoped construction error"));
                }
                Ok(())
            })
        }
        let mut call = Call {
            work: Some(work),
            result: None,
        };
        let frame = encode_telemetry(
            &PluginTelemetryScope {
                config: self.config.clone(),
                context: ExecutionContext::current().trace().cloned(),
            },
            MAX_TELEMETRY_RECORD_BYTES,
        )?;
        let table = self.table();
        let outcome = unsafe {
            let state = (&mut call as *mut Call<F, T>).cast();
            if let Some(query) = self.query {
                transport::take_status(query.with_scope.expect("validated")(
                    &table,
                    &self.query_table(),
                    abi::BorrowedBytes::new(&frame),
                    invoke::<F, T>,
                    state,
                ))
            } else {
                transport::take_status(self.extension.with_scope.expect("validated")(
                    &table,
                    abi::BorrowedBytes::new(&frame),
                    invoke::<F, T>,
                    state,
                ))
            }
        };
        match call.result {
            Some(Err(error)) => Err(error),
            Some(Ok(value)) => {
                outcome?;
                Ok(value)
            }
            None => {
                outcome?;
                anyhow::bail!("native telemetry scope did not invoke construction")
            }
        }
    }
}

pub(super) fn scope<T>(
    binding: Option<&NativeTelemetryBinding>,
    work: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    match binding {
        Some(binding) => binding.scope(work),
        None => work(),
    }
}

fn query_failure(state: &BindingState, error: TelemetryError) -> Failure {
    state.backend.diagnostics().record(match &error {
        TelemetryError::Capacity(_) => TelemetryLoss::Full,
        TelemetryError::Retired => TelemetryLoss::Closed,
        _ => TelemetryLoss::Invalid,
    });
    Failure::from(anyhow::Error::from(error))
}

fn query_record<T: TelemetryRecord + serde::de::DeserializeOwned>(
    state: &BindingState,
    bytes: abi::BorrowedBytes,
) -> drasi_lib::observability::Result<T> {
    let bytes =
        unsafe { transport::borrowed_bytes(bytes, state.backend.config().limits.record_bytes) }
            .map_err(|error| TelemetryError::Invalid(error.to_string()))?;
    decode_telemetry(bytes, state.backend.config().limits.record_bytes)
}

fn query_entry(
    state: &BindingState,
    token: QueryToken,
) -> drasi_lib::observability::Result<Arc<QueryEntry>> {
    if token.owner != state.query_owner || token.lease == 0 {
        return Err(TelemetryError::Invalid(
            "foreign native query observation token".into(),
        ));
    }
    state
        .queries
        .lock()
        .map_err(|_| TelemetryError::Poisoned)?
        .entries
        .get(&token.lease)
        .cloned()
        .ok_or_else(|| TelemetryError::Invalid("stale native query observation token".into()))
}

unsafe extern "C" fn begin_query(context: *mut c_void, bytes: abi::BorrowedBytes) -> abi::Reply {
    transport::reply_boundary(|| {
        let state = unsafe { state(context)? };
        let result = state.lifetime.with_active(|| {
            let request: QueryObservationRequest = query_record(state, bytes)?;
            state
                .query_count
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    (value < state.backend.config().limits.open_spans).then_some(value + 1)
                })
                .map_err(|_| TelemetryError::Capacity("native query observations"))?;
            OPEN_QUERY_OBSERVATIONS
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    (value < MAX_NATIVE_OPEN_SPANS).then_some(value + 1)
                })
                .map_err(|_| {
                    state.query_count.fetch_sub(1, Ordering::AcqRel);
                    TelemetryError::Capacity("native query observations")
                })?;
            let slot = QuerySlot(state.query_count.clone());
            let Some(observation) = state.backend.begin_query_observation(&request)? else {
                return wire::encode(
                    &Option::<drasi_computation_plugin_sdk::query_transport::QueryBeginReply>::None,
                )
                .map_err(|error| TelemetryError::Invalid(error.to_string()));
            };
            let entry = Arc::new(QueryEntry {
                observation: Mutex::new(Some(observation)),
                diagnostics: state.backend.diagnostics().clone(),
                _slot: slot,
            });
            let context = entry
                .observation
                .lock()
                .map_err(|_| TelemetryError::Poisoned)?
                .as_ref()
                .ok_or(TelemetryError::Retired)?
                .context();
            let encoded_context = context.encode()?;
            let mut queries = state.queries.lock().map_err(|_| TelemetryError::Poisoned)?;
            queries.next = queries.next.checked_add(1).ok_or(TelemetryError::Capacity(
                "native query observation identities",
            ))?;
            let token = QueryToken {
                owner: state.query_owner,
                lease: queries.next,
            };
            let reply = wire::encode(&Some(
                drasi_computation_plugin_sdk::query_transport::QueryBeginReply {
                    owner: token.owner,
                    lease: token.lease,
                    context: encoded_context,
                },
            ))
            .map_err(|error| TelemetryError::Invalid(error.to_string()))?;
            let mut processing = state
                .processing_context
                .lock()
                .map_err(|_| TelemetryError::Poisoned)?;
            if matches!(
                request.kind,
                QueryObservationKind::Bootstrap
                    | QueryObservationKind::FutureQueue
                    | QueryObservationKind::OutputReplay
                    | QueryObservationKind::InputReplay
            ) || (request.kind == QueryObservationKind::Evaluate && processing.is_none())
            {
                *processing = Some(context);
            }
            queries.entries.insert(token.lease, entry);
            Ok(reply)
        });
        result.map_err(|error| query_failure(state, error))
    })
}

unsafe extern "C" fn query_scope(
    context: *mut c_void,
    token: QueryToken,
    call: ScopedCall,
    callback_state: *mut c_void,
) -> abi::Status {
    transport::status_boundary(|| {
        let state = unsafe { state(context)? };
        let entry = state
            .lifetime
            .with_active(|| query_entry(state, token))
            .map_err(|error| query_failure(state, error))?;
        // A foreign callback can re-enter this binding; never keep the
        // revocation read gate across it when a retiring writer may be queued.
        (|| {
            if !state.lifetime.is_active()? {
                return Err(TelemetryError::Retired);
            }
            let observation = entry
                .observation
                .try_lock()
                .map_err(|_| TelemetryError::Invalid("native query observation is busy".into()))?;
            let observation = observation.as_ref().ok_or(TelemetryError::Retired)?;
            let mut called = false;
            let mut result = Ok(());
            observation.in_scope(&mut || {
                if called {
                    result = Err(TelemetryError::Invalid(
                        "query observation callback invoked twice".into(),
                    ));
                    return;
                }
                called = true;
                result = unsafe { transport::take_status(call(callback_state)) }
                    .map_err(|error| TelemetryError::Invalid(error.to_string()));
            })?;
            if !called {
                return Err(TelemetryError::Invalid(
                    "query observation callback not invoked".into(),
                ));
            }
            result
        })()
        .map_err(|error| query_failure(state, error))
    })
}

unsafe extern "C" fn finish_query(
    context: *mut c_void,
    token: QueryToken,
    bytes: abi::BorrowedBytes,
) -> abi::Status {
    transport::status_boundary(|| {
        let state = unsafe { state(context)? };
        state
            .lifetime
            .with_active(|| {
                let result: QueryObservationResult = query_record(state, bytes)?;
                let entry = query_entry(state, token)?;
                let mut queries = state.queries.lock().map_err(|_| TelemetryError::Poisoned)?;
                if !queries
                    .entries
                    .get(&token.lease)
                    .is_some_and(|value| Arc::ptr_eq(value, &entry))
                {
                    return Err(TelemetryError::Invalid(
                        "stale native query observation token".into(),
                    ));
                }
                queries.entries.remove(&token.lease);
                drop(queries);
                let mut observation = entry.observation.try_lock().map_err(|_| {
                    TelemetryError::Invalid(
                        "native query observation is busy; cancellation is pending".into(),
                    )
                })?;
                let mut observation = observation.take().ok_or(TelemetryError::Retired)?;
                observation.finish(result)
            })
            .map_err(|error| query_failure(state, error))
    })
}

unsafe extern "C" fn query_event(context: *mut c_void, bytes: abi::BorrowedBytes) -> abi::Status {
    transport::status_boundary(|| {
        let state = unsafe { state(context)? };
        state
            .lifetime
            .with_active(|| {
                state
                    .backend
                    .query_observation_event(query_record(state, bytes)?)
            })
            .map_err(|error| query_failure(state, error))
    })
}

unsafe fn state<'a>(context: *mut c_void) -> std::result::Result<&'a BindingState, Failure> {
    if context.is_null() {
        return Err(Failure::protocol("null native telemetry binding"));
    }
    Ok(unsafe { &*context.cast::<BindingState>() })
}
unsafe extern "C" fn retain(context: *mut c_void) {
    transport::drop_boundary(|| {
        if context.is_null() {
            log::error!("Retaining null native telemetry binding");
        } else {
            unsafe { Arc::increment_strong_count(context.cast::<BindingState>()) };
        }
    });
}
unsafe extern "C" fn release(context: *mut c_void) {
    transport::drop_boundary(|| {
        if context.is_null() {
            log::error!("Releasing null native telemetry binding");
        } else {
            unsafe { Arc::decrement_strong_count(context.cast::<BindingState>()) };
        }
    });
}

fn submission(context: *mut c_void, bytes: abi::BorrowedBytes, span: bool) -> u32 {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let state = unsafe { state(context)? };
        let value = state.lifetime.with_active(|| {
            let bytes = unsafe {
                transport::borrowed_bytes(bytes, state.backend.config().limits.record_bytes)
            }
            .map_err(|error| TelemetryError::Invalid(error.to_string()))?;
            if span {
                Ok(state.backend.submit_encoded_span(bytes))
            } else {
                let record = decode_telemetry::<TelemetryLogRecord>(
                    bytes,
                    state.backend.config().limits.record_bytes,
                )?;
                Ok(state.backend.submit_log(&record))
            }
        });
        if value.is_err() {
            state.backend.diagnostics().record(TelemetryLoss::Invalid);
        }
        Ok::<_, Failure>(value.unwrap_or(TelemetrySubmission::Invalid))
    }));
    match result {
        Ok(Ok(outcome)) => outcome as u32,
        Ok(Err(error)) => {
            log::error!("Invalid native telemetry callback: {error}");
            TelemetrySubmission::Invalid as u32
        }
        Err(_) => {
            if let Ok(state) = unsafe { state(context) } {
                state.backend.diagnostics().record(TelemetryLoss::Invalid);
            }
            log::error!("Native telemetry submission callback panicked");
            TelemetrySubmission::Invalid as u32
        }
    }
}
unsafe extern "C" fn submit_span(context: *mut c_void, bytes: abi::BorrowedBytes) -> u32 {
    submission(context, bytes, true)
}
unsafe extern "C" fn submit_log(context: *mut c_void, bytes: abi::BorrowedBytes) -> u32 {
    submission(context, bytes, false)
}

unsafe extern "C" fn register_metric(
    context: *mut c_void,
    bytes: abi::BorrowedBytes,
) -> abi::Reply {
    transport::reply_boundary(|| {
        let state = unsafe { state(context)? };
        let result = state.lifetime.with_active(|| {
            let bytes = unsafe {
                transport::borrowed_bytes(bytes, state.backend.config().limits.record_bytes)
            }
            .map_err(|error| TelemetryError::Invalid(error.to_string()))?;
            let request = decode_telemetry::<PluginMetricRegistration>(
                bytes,
                state.backend.config().limits.record_bytes,
            )?;
            state.backend.register_metric_handle(&request)
        });
        if result.is_err() {
            state.backend.diagnostics().record(TelemetryLoss::Metric);
        }
        wire::encode(&result.map_err(anyhow::Error::from)?).map_err(Failure::from)
    })
}
unsafe extern "C" fn update_metric(context: *mut c_void, bytes: abi::BorrowedBytes) -> abi::Status {
    transport::status_boundary(|| {
        let state = unsafe { state(context)? };
        let result = state.lifetime.with_active(|| {
            let bytes = unsafe {
                transport::borrowed_bytes(bytes, state.backend.config().limits.record_bytes)
            }
            .map_err(|error| TelemetryError::Invalid(error.to_string()))?;
            let update = decode_telemetry::<PluginMetricUpdate>(
                bytes,
                state.backend.config().limits.record_bytes,
            )?;
            state.backend.update_metric(update)
        });
        if result.is_err() {
            state.backend.diagnostics().record(TelemetryLoss::Metric);
        }
        result.map_err(|error| Failure::from(anyhow::Error::from(error)))
    })
}
unsafe extern "C" fn sample_root(context: *mut c_void, bytes: abi::BorrowedBytes) -> abi::Reply {
    transport::reply_boundary(|| {
        let state = unsafe { state(context)? };
        let result = state.lifetime.with_active(|| {
            let bytes = unsafe {
                transport::borrowed_bytes(bytes, state.backend.config().limits.record_bytes)
            }
            .map_err(|error| TelemetryError::Invalid(error.to_string()))?;
            let request = decode_telemetry::<RootSpanRequest>(
                bytes,
                state.backend.config().limits.record_bytes,
            )?;
            let decision = state.backend.sample_root(&request)?;
            encode_telemetry(&decision, state.backend.config().limits.record_bytes)
        });
        if result.is_err() {
            state.backend.diagnostics().record(TelemetryLoss::Invalid);
        }
        result.map_err(|error| Failure::from(anyhow::Error::from(error)))
    })
}
