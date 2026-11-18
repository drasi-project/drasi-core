// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_plugin_abi::{self as abi, telemetry::*};
use drasi_lib::observability::*;
use std::{ffi::c_void, sync::Arc};

#[derive(Debug)]
pub(super) struct HostBridge {
    resolver: Arc<dyn HandledBranchTransport>,
    lifetime: Option<TelemetryLifetime>,
}

impl HostBridge {
    pub(super) fn new(contexts: impl IntoIterator<Item = ExecutionContext>) -> Option<Arc<Self>> {
        contexts
            .into_iter()
            .find_map(|context| match context.handled_branch() {
                Some(HandledBranchContext::Tracked(lease)) => lease.transport().map(|resolver| {
                    Arc::new(Self {
                        resolver,
                        lifetime: ExecutionScope::current_timing_lifetime(),
                    })
                }),
                _ => None,
            })
    }

    pub(super) fn table(self: &Arc<Self>) -> HostTimingV3 {
        HostTimingV3 {
            header: abi::Header::new::<HostTimingV3>(),
            version: VERSION_V3,
            reserved: 0,
            context: Arc::as_ptr(self).cast_mut().cast(),
            retain_context: Some(retain),
            release_context: Some(release),
            acquire: Some(acquire),
            release: Some(release_token),
        }
    }
}

impl HandledBranchTransport for HostBridge {
    fn acquire(&self, token: HandledBranchToken) -> Result<HandledBranchContext> {
        self.active(|| self.resolver.acquire(token))
    }
}

impl HostBridge {
    fn active<T>(&self, work: impl FnOnce() -> Result<T>) -> Result<T> {
        match &self.lifetime {
            Some(lifetime) => lifetime.with_active(work),
            None => work(),
        }
    }
}

unsafe extern "C" fn retain(context: *mut c_void) {
    unsafe { Arc::increment_strong_count(context.cast::<HostBridge>()) };
}
unsafe extern "C" fn release(context: *mut c_void) {
    unsafe { Arc::decrement_strong_count(context.cast::<HostBridge>()) };
}
fn token(token: TimingToken) -> HandledBranchToken {
    HandledBranchToken {
        owner: token.owner,
        lease: token.lease,
    }
}
fn code(error: &TelemetryError) -> u32 {
    match error {
        TelemetryError::Capacity(_) => abi::status::BUSY,
        TelemetryError::Retired => abi::status::CLOSED,
        _ => abi::status::PROTOCOL,
    }
}
unsafe extern "C" fn acquire(
    context: *mut c_void,
    input: TimingToken,
    output: *mut TimingToken,
) -> u32 {
    if context.is_null() || output.is_null() {
        return abi::status::PROTOCOL;
    }
    let bridge = unsafe { &*context.cast::<HostBridge>() };
    match bridge.active(|| bridge.resolver.retain_token(token(input))) {
        Ok(token) => {
            unsafe {
                output.write(TimingToken {
                    owner: token.owner,
                    lease: token.lease,
                })
            };
            abi::status::OK
        }
        Err(error) => {
            log::warn!("Native timing lease acquisition rejected: {error}");
            code(&error)
        }
    }
}
unsafe extern "C" fn release_token(context: *mut c_void, input: TimingToken) -> u32 {
    if context.is_null() {
        return abi::status::PROTOCOL;
    }
    let bridge = unsafe { &*context.cast::<HostBridge>() };
    match bridge.resolver.release_token(token(input)) {
        Ok(()) => abi::status::OK,
        Err(error) => {
            log::error!("Native timing lease release rejected: {error}");
            code(&error)
        }
    }
}
