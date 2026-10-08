// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional host-owned consumer progress. The plugin handles operations, but
//! cannot advance progress, commit the host transaction or acknowledge transport.
//! Each batch crosses once; subsequent requests contain generation/index/identity.

use super::*;

pub const SYMBOL: &[u8] = b"drasi_computation_plugin_consumer_v1\0";
pub const VERSION: u32 = 1;
pub const BEGIN_BATCH: u32 = 1;
pub const HANDLE: u32 = 2;
pub const RETRYABLE: u32 = 0x1001;
pub const MAX_CONTROL_BYTES: usize = 4096;
pub type ConsumerFn = unsafe extern "C" fn() -> *const PluginConsumerV1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginConsumerV1 {
    pub header: Header,
    pub version: u32,
    pub reserved: u32,
    pub factory: Option<unsafe extern "C" fn(*mut c_void, u32) -> Reply>,
    pub create:
        Option<unsafe extern "C" fn(*mut c_void, BorrowedBytes, *mut ComponentHandle) -> Status>,
    pub inspect: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
    pub begin: Option<
        unsafe extern "C" fn(
            *mut c_void,
            u32,
            BorrowedBytes,
            *const Transaction,
            *mut OperationHandle,
        ) -> Status,
    >,
    /// Revokes only the named batch. Returns BUSY while an operation is active.
    /// Dropping a batch does not acknowledge input.
    pub end_batch: Option<unsafe extern "C" fn(*mut c_void, u64) -> Status>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn consumer_extension_preserves_frozen_native_tables() {
        let pointer = size_of::<usize>();
        assert_eq!(size_of::<PluginConsumerV1>(), 24 + 5 * pointer);
        assert_eq!(size_of::<ComponentVTable>(), 16 + 5 * pointer);
        assert_eq!(size_of::<Transaction>(), 16 + 4 * pointer);
        assert_eq!(ABI_VERSION, "1.0.0");
        assert_eq!(WIRE_VERSION, 2);
    }
}
