// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional read-only snapshots of graph outputs (a source, transformer or query
//! output port). The host provides the capability to a component; each reader
//! owns its cancellation and release independently. No catalogue, graph control
//! or Rust object crosses this ABI: requests and replies are bounded
//! MessagePack records.

use crate::*;

pub const VERSION: u32 = 1;
pub const SYMBOL: &[u8] = b"drasi_computation_plugin_output_snapshots_v1\0";
/// Largest accepted open request, in bytes.
pub const MAX_OPEN_BYTES: usize = 4096;
pub type OutputSnapshotsFn = unsafe extern "C" fn() -> *const PluginOutputSnapshotsV1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginOutputSnapshotsV1 {
    pub header: Header,
    pub version: u32,
    pub reserved: u32,
    pub factory: Option<unsafe extern "C" fn(*mut c_void, u32) -> Reply>,
    pub create: Option<
        unsafe extern "C" fn(
            *mut c_void,
            BorrowedBytes,
            *const OutputSnapshotsV1,
            *mut ComponentHandle,
        ) -> Status,
    >,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct OutputSnapshotsV1 {
    pub header: Header,
    pub context: *mut c_void,
    pub retain: Option<unsafe extern "C" fn(*mut c_void)>,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    /// Validates a bounded MessagePack open request naming one output endpoint
    /// (component and port) and allocates a bounded reader. Opening a reader
    /// performs no I/O; PREPARE captures its snapshot.
    pub open: Option<
        unsafe extern "C" fn(*mut c_void, BorrowedBytes, *mut OutputSnapshotHandle) -> Status,
    >,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct OutputSnapshotHandle {
    pub state: *mut c_void,
    pub vtable: *const OutputSnapshotVTable,
}
impl OutputSnapshotHandle {
    pub const fn null() -> Self {
        Self {
            state: std::ptr::null_mut(),
            vtable: std::ptr::null(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct OutputSnapshotVTable {
    pub header: Header,
    /// Cancels pending operations and releases this reader's ownership.
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    pub request: Option<unsafe extern "C" fn(*mut c_void, u32, *mut OperationHandle) -> Status>,
}

pub mod operation {
    pub const PREPARE: u32 = 1;
    pub const NEXT: u32 = 2;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_extension_does_not_change_base_layouts() {
        let pointer = size_of::<usize>();
        assert_eq!(size_of::<PluginOutputSnapshotsV1>(), 24 + 2 * pointer);
        assert_eq!(size_of::<OutputSnapshotsV1>(), 16 + 4 * pointer);
        assert_eq!(size_of::<OutputSnapshotHandle>(), 2 * pointer);
        assert_eq!(size_of::<OutputSnapshotVTable>(), 16 + 2 * pointer);
        assert_eq!(size_of::<ComponentVTable>(), 16 + 5 * pointer);
        assert_eq!(ABI_VERSION, "1.0.0");
        assert_eq!(WIRE_VERSION, 1);
    }
}
