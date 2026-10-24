// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional query-owned bootstrap providers. This extension does not change the
//! base component roles, metadata, tables or source-progress extension.
//!
//! Factories are configuration-only. Operations are caller-polled and serialized
//! per provider. NEXT transfers one envelope, never an unbounded snapshot.
//! Cancelling/dropping a stream requests cancellation; STOP must finish cleanup
//! before the owner releases storage or constructs a replacement.

use crate::{recovery::SourceProgressV1, BorrowedBytes, Header, OperationHandle, Reply, Status};
use std::{ffi::c_void, ptr};

pub const SYMBOL: &[u8] = b"drasi_computation_plugin_bootstrap_v1\0";
pub const VERSION: u32 = 1;
pub const MAX_CONTROL_BYTES: usize = 1024 * 1024;
pub const MAX_STATE_BYTES: usize = 65_536;
pub const MAX_WATERMARKS: usize = 4096;
pub const PREPARE: u32 = 1;
pub const SNAPSHOT: u32 = 2;
pub const NEXT: u32 = 3;
pub const COMPLETE: u32 = 4;
pub const STOP: u32 = 5;
pub const READ_STATE: u32 = 101;
pub const WRITE_STATE: u32 = 102;

pub type BootstrapFn = unsafe extern "C" fn() -> *const PluginBootstrapV1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginBootstrapV1 {
    pub header: Header,
    pub version: u32,
    pub reserved: u32,
    pub factories: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
    pub create: Option<
        unsafe extern "C" fn(
            *mut c_void,
            u32,
            BorrowedBytes,
            *const SourceProgressV1,
            *mut BootstrapHandle,
        ) -> Status,
    >,
}

/// Borrowed only by PREPARE/SNAPSHOT. The request carrier reuses the revocable
/// mailbox layout, not transaction semantics: only READ_STATE/WRITE_STATE exist.
/// Retention extends callback memory lifetime, never the host-state borrow.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootstrapStateV1 {
    pub header: Header,
    pub durability: BorrowedBytes,
    pub reserved: u32,
    pub requests: crate::Transaction,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootstrapHandle {
    pub state: *mut c_void,
    pub vtable: *const BootstrapVTable,
}
impl BootstrapHandle {
    pub const fn null() -> Self {
        Self {
            state: ptr::null_mut(),
            vtable: ptr::null(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootstrapVTable {
    pub header: Header,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    pub begin: Option<
        unsafe extern "C" fn(
            *mut c_void,
            u32,
            u64,
            *const BootstrapStateV1,
            *mut OperationHandle,
        ) -> Status,
    >,
    /// Thread-safe, nonblocking cancellation request, not proof of cleanup.
    /// Zero cancels the provider's active operation; a nonzero snapshot generation
    /// must match before cancellation can affect its stream.
    pub cancel_snapshot: Option<unsafe extern "C" fn(*mut c_void, u64) -> Status>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn bootstrap_extension_preserves_frozen_native_tables() {
        let pointer = size_of::<usize>();
        assert_eq!(size_of::<PluginBootstrapV1>(), 24 + 2 * pointer);
        assert_eq!(size_of::<BootstrapHandle>(), 2 * pointer);
        assert_eq!(size_of::<BootstrapVTable>(), 16 + 3 * pointer);
        assert_eq!(size_of::<crate::ComponentVTable>(), 16 + 5 * pointer);
        assert_eq!(size_of::<crate::Transaction>(), 16 + 4 * pointer);
        assert_eq!(crate::ABI_VERSION, "1.0.0");
        assert_eq!(crate::WIRE_VERSION, 2);
    }
}
