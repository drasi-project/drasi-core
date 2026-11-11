// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Versioned telemetry extensions. V1 and all base tables/wire formats are frozen.

use crate::{BorrowedBytes, Header, OperationHandle, Status, Transaction};
use std::ffi::c_void;

pub const VERSION: u32 = 1;
pub const CONTEXT: u32 = 1;
pub const SYMBOL: &[u8] = b"drasi_computation_plugin_telemetry_v1\0";
pub const MAX_CONTEXTS: usize = 65_536;
/// Negotiated context-bearing admission, distinct from the frozen ADMIT payload.
pub const ADMIT: u32 = 0x10001;

pub type TelemetryFn = unsafe extern "C" fn() -> *const PluginTelemetryV1;
pub type Begin = unsafe extern "C" fn(
    *mut c_void,
    u32,
    BorrowedBytes,
    *const Transaction,
    *mut OperationHandle,
) -> Status;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PluginTelemetryV1 {
    pub header: Header,
    pub version: u32,
    pub capabilities: u32,
    pub reserved: u64,
    pub begin: Option<Begin>,
    pub begin_consumer: Option<Begin>,
}

pub const VERSION_V2: u32 = 2;
pub const SYMBOL_V2: &[u8] = b"drasi_computation_plugin_telemetry_v2\0";
pub const SPANS: u32 = 2;
pub const LOGS: u32 = 4;
pub const METRICS: u32 = 8;
pub const ALL: u32 = CONTEXT | SPANS | LOGS | METRICS;
pub type TelemetryFnV2 = unsafe extern "C" fn() -> *const PluginTelemetryV2;
pub type ScopedCall = unsafe extern "C" fn(*mut c_void) -> Status;
pub type WithScope =
    unsafe extern "C" fn(*const HostTelemetryV2, BorrowedBytes, ScopedCall, *mut c_void) -> Status;
pub type Submit = unsafe extern "C" fn(*mut c_void, BorrowedBytes) -> u32;
pub type Request = unsafe extern "C" fn(*mut c_void, BorrowedBytes) -> crate::Reply;
pub type Update = unsafe extern "C" fn(*mut c_void, BorrowedBytes) -> Status;

/// `call` is synchronous and may not be retained. It invokes existing
/// service-aware constructors while the plugin-local telemetry scope is entered.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PluginTelemetryV2 {
    pub header: Header,
    pub version: u32,
    pub capabilities: u32,
    pub reserved: u64,
    pub begin: Option<Begin>,
    pub begin_consumer: Option<Begin>,
    pub with_scope: Option<WithScope>,
}

/// The plugin retains `context` before the borrowing call returns. Only the
/// host dereferences it; callbacks may run concurrently until the last release.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct HostTelemetryV2 {
    pub header: Header,
    pub version: u32,
    pub capabilities: u32,
    pub context: *mut c_void,
    pub retain: Option<unsafe extern "C" fn(*mut c_void)>,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    pub submit_span: Option<Submit>,
    pub submit_log: Option<Submit>,
    pub register_metric: Option<Request>,
    pub update_metric: Option<Update>,
    pub sample_root: Option<Request>,
}

pub const VERSION_V3: u32 = 3;
pub const SYMBOL_V3: &[u8] = b"drasi_computation_plugin_telemetry_v3\0";
pub const HANDLED_TIMING: u32 = 16;
pub type TelemetryFnV3 = unsafe extern "C" fn() -> *const PluginTelemetryV3;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TimingToken {
    pub owner: [u8; 16],
    pub lease: u64,
}

pub type RetainTiming = unsafe extern "C" fn(*mut c_void, TimingToken, *mut TimingToken) -> u32;
pub type ReleaseTiming = unsafe extern "C" fn(*mut c_void, TimingToken) -> u32;

/// The plugin retains callback context before storing it, and acquires a distinct
/// host lease for each imported carrier. Reply buffers retain output leases until
/// the host has imported the carrier or released the buffer.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct HostTimingV3 {
    pub header: Header,
    pub version: u32,
    pub reserved: u32,
    pub context: *mut c_void,
    pub retain_context: Option<unsafe extern "C" fn(*mut c_void)>,
    pub release_context: Option<unsafe extern "C" fn(*mut c_void)>,
    pub acquire: Option<RetainTiming>,
    pub release: Option<ReleaseTiming>,
}

pub type BeginTiming = unsafe extern "C" fn(
    *mut c_void,
    u32,
    BorrowedBytes,
    *const Transaction,
    *const HostTimingV3,
    *mut OperationHandle,
) -> Status;

/// Optional next version in the existing telemetry family. Base/V1/V2 tables
/// and their frame meanings stay unchanged.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PluginTelemetryV3 {
    pub header: Header,
    pub version: u32,
    pub capabilities: u32,
    pub reserved: u64,
    pub begin: Option<BeginTiming>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn context_extension_preserves_frozen_base_layouts() {
        use std::mem::size_of;
        let pointer = size_of::<usize>();
        assert_eq!(size_of::<super::PluginTelemetryV1>(), 32 + 2 * pointer);
        assert_eq!(size_of::<super::PluginTelemetryV2>(), 32 + 3 * pointer);
        assert_eq!(size_of::<super::HostTelemetryV2>(), 24 + 8 * pointer);
        assert_eq!(size_of::<super::PluginTelemetryV3>(), 32 + pointer);
        assert_eq!(size_of::<super::HostTimingV3>(), 24 + 5 * pointer);
        assert_eq!(size_of::<crate::ComponentVTable>(), 16 + 5 * pointer);
        assert_eq!(size_of::<crate::FactoryVTable>(), 16 + 3 * pointer);
        assert_eq!(size_of::<crate::OperationVTable>(), 16 + 3 * pointer);
        assert_eq!(crate::ABI_VERSION, "2.0.0");
        assert_eq!(crate::WIRE_VERSION, 2);
    }
}
