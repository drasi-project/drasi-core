// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional per-output snapshot and replay reads. A component declares exactly the
//! output ports for which it registered a provider; there is no capability flag
//! and no default no-op callback. Providers are opened once per declared
//! (port, capability), at construction, and are read independently of the
//! component's data/lifecycle operation slot. No Rust trait, `Arc`, `Bytes` or
//! borrowed pointer crosses this interface.

use crate::*;

pub const VERSION: u32 = 1;
pub const SYMBOL: &[u8] = b"drasi_computation_plugin_output_reads_v1\0";
pub type OutputReadsFn = unsafe extern "C" fn() -> *const PluginOutputReadsV1;

/// Complete encoded snapshot or replay reply. A larger answer is an explicit limit
/// outcome, never a truncated one.
pub const MAX_READ_BYTES: usize = 16 * 1024 * 1024;
/// Encoded replay cursor request.
pub const MAX_REQUEST_BYTES: usize = 16 * 1024;
/// Records in one snapshot.
pub const MAX_SNAPSHOT_RECORDS: usize = 16 * 1024;
/// Envelopes in one replay.
pub const MAX_REPLAY_ENVELOPES: usize = 1024;
/// Reads in flight across one component's providers; excess reads report BUSY.
pub const MAX_CONCURRENT_READS: usize = 16;
const _: () = assert!(MAX_READ_BYTES < MAX_MESSAGE_BYTES);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginOutputReadsV1 {
    pub header: Header,
    pub version: u32,
    pub reserved: u32,
    /// Opens one provider handle on a component created by the base factory. The
    /// bytes are a UTF-8 output port id; `capability` is a `capability` value. The
    /// port/capability must be declared in factory metadata and registered by the
    /// constructed component. Opening performs no I/O.
    pub open: Option<
        unsafe extern "C" fn(*mut c_void, BorrowedBytes, u32, *mut OutputReadHandle) -> Status,
    >,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct OutputReadHandle {
    pub state: *mut c_void,
    pub vtable: *const OutputReadVTable,
}
impl OutputReadHandle {
    pub const fn null() -> Self {
        Self {
            state: std::ptr::null_mut(),
            vtable: std::ptr::null(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct OutputReadVTable {
    pub header: Header,
    /// Cancels this handle's pending operations and releases its ownership. The
    /// owning component's cleanup does not release it; the holder must.
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    /// Borrowed input is copied before returning. The operation's reply is
    /// bounded MessagePack, owned by the producer until released. Provider
    /// results, including typed provider errors, are in the reply; a nonzero
    /// status describes lifecycle or transport failure.
    pub request: Option<
        unsafe extern "C" fn(*mut c_void, u32, BorrowedBytes, *mut OperationHandle) -> Status,
    >,
}

pub mod capability {
    pub const SNAPSHOT: u32 = 1;
    pub const REPLAY: u32 = 2;
}

pub mod operation {
    /// Snapshot handle; empty input.
    pub const SNAPSHOT: u32 = 1;
    /// Replay handle; empty input.
    pub const COMMITTED_HEAD: u32 = 2;
    /// Replay handle; input is an encoded cursor.
    pub const REPLAY_AFTER: u32 = 3;
}

pub mod status {
    /// The component has not completed START, or stopped, but is still current. A
    /// later activation permits new reads; it never revives an earlier read.
    pub const NOT_ACTIVE: u32 = 0x2001;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_read_extension_preserves_base_layouts() {
        let pointer = size_of::<usize>();
        assert_eq!(size_of::<PluginOutputReadsV1>(), 24 + pointer);
        assert_eq!(size_of::<OutputReadHandle>(), 2 * pointer);
        assert_eq!(size_of::<OutputReadVTable>(), 16 + 2 * pointer);
        assert_eq!(size_of::<ComponentVTable>(), 16 + 5 * pointer);
        assert_eq!(size_of::<OperationVTable>(), 16 + 3 * pointer);
        assert_eq!(ABI_VERSION, "1.0.0");
        assert_eq!(WIRE_VERSION, 1);
    }
}
