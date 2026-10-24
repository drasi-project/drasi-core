// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional read-only source recovery extension. Base ABI and service-v1 tables
//! are unchanged. All handles obey the base producer-side ownership rules.

use super::*;

pub const SYMBOL: &[u8] = b"drasi_computation_plugin_recovery_v1\0";
pub const VERSION: u32 = 1;
pub const MAX_PROGRESS_BYTES: usize = 1024 * 1024;
pub const MAX_PROGRESS_ENTRIES: usize = 4096;
pub const MAX_SUBSCRIPTIONS: usize = 16;
pub type RecoveryFn = unsafe extern "C" fn() -> *const PluginRecoveryV1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginRecoveryV1 {
    pub header: Header,
    pub version: u32,
    pub reserved: u32,
    pub factory: Option<unsafe extern "C" fn(*mut c_void, u32) -> Reply>,
    pub create: Option<
        unsafe extern "C" fn(
            *mut c_void,
            BorrowedBytes,
            *const SourceProgressV1,
            *mut ComponentHandle,
        ) -> Status,
    >,
    /// Immutable instance declaration, not proof of a host transaction owner.
    pub inspect: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
}

/// Borrowed host capability. Retain before storing. Synchronous reads are
/// bounded and perform no I/O. No write, acknowledge, reset or commit operation.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SourceProgressV1 {
    pub header: Header,
    pub context: *mut c_void,
    pub retain: Option<unsafe extern "C" fn(*mut c_void)>,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    pub describe: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
    pub snapshot: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
    pub subscribe: Option<unsafe extern "C" fn(*mut c_void, *mut ProgressSubscriptionV1) -> Status>,
}

/// Owned subscription, transferred on successful subscribe. Release exactly once.
/// Reads mark state observed. Wait returns empty success when state differs from
/// the last observation; cancelling a wait never consumes an observation.
/// One pending wait per subscription; all reads/waits fail after revocation.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProgressSubscriptionV1 {
    pub header: Header,
    pub context: *mut c_void,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    pub snapshot: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
    pub wait: Option<unsafe extern "C" fn(*mut c_void, *mut OperationHandle) -> Status>,
}

impl ProgressSubscriptionV1 {
    pub const fn null() -> Self {
        Self {
            header: Header::new::<Self>(),
            context: ptr::null_mut(),
            release: None,
            snapshot: None,
            wait: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_extension_does_not_change_existing_tables() {
        let pointer = size_of::<usize>();
        assert_eq!(size_of::<PluginRecoveryV1>(), 24 + 3 * pointer);
        assert_eq!(size_of::<SourceProgressV1>(), 16 + 6 * pointer);
        assert_eq!(size_of::<ProgressSubscriptionV1>(), 16 + 4 * pointer);
        assert_eq!(size_of::<services::PluginServicesV1>(), 24 + 2 * pointer);
        assert_eq!(size_of::<ComponentVTable>(), 16 + 5 * pointer);
        assert_eq!(ABI_VERSION, "1.0.0");
        assert_eq!(WIRE_VERSION, 2);
    }
}
