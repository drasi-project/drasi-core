// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Internal source/bootstrap pairing. The source library owns the coordinator;
//! paired libraries obtain existing bootstrap handles, never SQL access.

use crate::*;

pub const VERSION: u32 = 1;
pub const SYMBOL: &[u8] = b"drasi_computation_plugin_bootstrap_coordination_v1\0";
pub const MAX_MEMBERS: usize = 256;
pub type BootstrapCoordinationFn = unsafe extern "C" fn() -> *const PluginBootstrapCoordinationV1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginBootstrapCoordinationV1 {
    pub header: Header,
    pub version: u32,
    pub reserved: u32,
    pub source_factory: Option<unsafe extern "C" fn(*mut c_void, u32) -> Reply>,
    pub bootstrap_factory: Option<unsafe extern "C" fn(*mut c_void, u32) -> Reply>,
    pub create: Option<
        unsafe extern "C" fn(
            *mut c_void,
            BorrowedBytes,
            *const BootstrapProgressV1,
            *mut BootstrapCoordinatorHandle,
        ) -> Status,
    >,
    pub create_source: Option<
        unsafe extern "C" fn(
            *mut c_void,
            BorrowedBytes,
            *const BootstrapCoordinatorHandle,
            *const recovery::SourceProgressV1,
            *const consumer_progress::ConsumerProgressV1,
            *mut ComponentHandle,
        ) -> Status,
    >,
    pub create_bootstrap: Option<
        unsafe extern "C" fn(
            *mut c_void,
            u32,
            BorrowedBytes,
            *const SourceBootstrapV1,
            *mut bootstrap::BootstrapHandle,
        ) -> Status,
    >,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootstrapProgressMemberV1 {
    pub id: BorrowedBytes,
    pub progress: recovery::SourceProgressV1,
}

/// Either one single owner or a group and all of its real member readers.
/// Tables and member arrays are borrowed only during construction.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootstrapProgressV1 {
    pub header: Header,
    pub single: *const recovery::SourceProgressV1,
    pub consumers: *const consumer_progress::ConsumerProgressV1,
    pub members: *const BootstrapProgressMemberV1,
    pub member_count: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootstrapCoordinatorHandle {
    pub state: *mut c_void,
    pub vtable: *const BootstrapCoordinatorVTable,
}

impl BootstrapCoordinatorHandle {
    pub const fn null() -> Self {
        Self {
            state: std::ptr::null_mut(),
            vtable: std::ptr::null(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootstrapCoordinatorVTable {
    pub header: Header,
    pub retain: Option<unsafe extern "C" fn(*mut c_void)>,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    pub describe: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
    pub open: Option<
        unsafe extern "C" fn(*mut c_void, BorrowedBytes, *mut bootstrap::BootstrapHandle) -> Status,
    >,
    pub stop: Option<unsafe extern "C" fn(*mut c_void, *mut OperationHandle) -> Status>,
}

/// A host-validated pairing to exactly one query and actual progress owner.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SourceBootstrapV1 {
    pub header: Header,
    pub context: *mut c_void,
    pub retain: Option<unsafe extern "C" fn(*mut c_void)>,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    pub describe: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
    pub open: Option<unsafe extern "C" fn(*mut c_void, *mut bootstrap::BootstrapHandle) -> Status>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_extension_preserves_existing_layouts() {
        let pointer = size_of::<usize>();
        assert_eq!(size_of::<PluginBootstrapCoordinationV1>(), 24 + 5 * pointer);
        assert_eq!(size_of::<BootstrapCoordinatorHandle>(), 2 * pointer);
        assert_eq!(size_of::<BootstrapCoordinatorVTable>(), 16 + 5 * pointer);
        assert_eq!(size_of::<SourceBootstrapV1>(), 16 + 5 * pointer);
        assert_eq!(size_of::<ComponentVTable>(), 16 + 5 * pointer);
        assert_eq!(ABI_VERSION, "1.0.0");
    }
}
