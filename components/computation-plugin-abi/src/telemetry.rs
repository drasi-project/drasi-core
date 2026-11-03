// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional execution-context transport. Base tables and wire version stay frozen.
//! The extension accepts bounded MessagePack operation capsules around unchanged
//! base payloads. It advertises no span/log collection or metric callbacks.

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

#[cfg(test)]
mod tests {
    #[test]
    fn context_extension_preserves_frozen_base_layouts() {
        use std::mem::size_of;
        let pointer = size_of::<usize>();
        assert_eq!(size_of::<super::PluginTelemetryV1>(), 32 + 2 * pointer);
        assert_eq!(size_of::<crate::ComponentVTable>(), 16 + 5 * pointer);
        assert_eq!(size_of::<crate::FactoryVTable>(), 16 + 2 * pointer);
        assert_eq!(size_of::<crate::OperationVTable>(), 16 + 3 * pointer);
        assert_eq!(crate::ABI_VERSION, "1.0.0");
        assert_eq!(crate::WIRE_VERSION, 1);
    }
}
