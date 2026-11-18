// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_plugin_abi::telemetry::{METRICS, SPANS};
use drasi_computation_plugin_abi::{self as abi, telemetry::*};
use drasi_computation_plugin_sdk::{
    transport::{self, Failure},
    wire,
};
use drasi_lib::observability::*;
use std::{ffi::c_void, sync::Arc};

struct BindingState {
    backend: PluginTelemetry,
    lifetime: TelemetryLifetime,
}

pub(super) struct NativeTelemetryBinding {
    state: Arc<BindingState>,
    extension: PluginTelemetryV2,
    config: PluginTelemetryConfig,
    capabilities: u32,
}

impl Drop for NativeTelemetryBinding {
    fn drop(&mut self) {
        if let Err(error) = self.state.lifetime.revoke(|| {}) {
            log::error!("Revoking native telemetry callbacks: {error}");
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
        Ok(Some(Self {
            state: Arc::new(BindingState {
                backend,
                lifetime: Default::default(),
            }),
            extension,
            config,
            capabilities,
        }))
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
            transport::take_status(self.extension.with_scope.expect("validated")(
                &table,
                abi::BorrowedBytes::new(&frame),
                invoke::<F, T>,
                (&mut call as *mut Call<F, T>).cast(),
            ))
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
