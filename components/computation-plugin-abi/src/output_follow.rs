// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional per-output coherent snapshot plus bounded live tail. It is separate
//! from `output_reads` only because a lease has its own handle: it exists as soon
//! as it is requested, serves any number of independent, cancellable `next`
//! operations and is released exactly once by its holder, which synchronously
//! ends the producer's retention obligation. Messages reuse the output-read wire
//! records and bounds. No Rust trait, `Arc`, `Bytes` or borrowed pointer crosses
//! this interface, and the extension does not claim crash durability or a
//! generic follow coordinator.

use crate::*;

pub const VERSION: u32 = 1;
pub const SYMBOL: &[u8] = b"drasi_computation_plugin_output_follow_v1\0";
pub type OutputFollowFn = unsafe extern "C" fn() -> *const PluginOutputFollowV1;

/// Leases that may exist at once across one component's providers.
pub const MAX_LEASES: usize = 16;
/// Encoded `begin` limits and `next` requests.
pub const MAX_REQUEST_BYTES: usize = 1024;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginOutputFollowV1 {
    pub header: Header,
    pub version: u32,
    pub reserved: u32,
    /// Opens a provider handle for a declared output port of a component created
    /// by the base factory. The port must be declared in factory metadata
    /// (`output_follows`) and registered by the constructed component. Opening
    /// performs no I/O.
    pub open: Option<
        unsafe extern "C" fn(*mut c_void, BorrowedBytes, *mut OutputFollowProviderHandle) -> Status,
    >,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct OutputFollowProviderHandle {
    pub state: *mut c_void,
    pub vtable: *const OutputFollowProviderVTable,
}
impl OutputFollowProviderHandle {
    pub const fn null() -> Self {
        Self {
            state: std::ptr::null_mut(),
            vtable: std::ptr::null(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct OutputFollowProviderVTable {
    pub header: Header,
    /// Releases this handle. Leases already opened through it stay valid until
    /// their own release or retirement.
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    /// Borrowed limits are copied before returning. On success both out values
    /// are written: a lease handle that exists immediately, and the operation
    /// that opens it. The operation's reply is the encoded snapshot; until it
    /// completes the lease serves no `next`. Releasing the lease cancels its
    /// pending open. On failure neither is written.
    pub begin: Option<
        unsafe extern "C" fn(
            *mut c_void,
            BorrowedBytes,
            *mut OutputFollowLeaseHandle,
            *mut OperationHandle,
        ) -> Status,
    >,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct OutputFollowLeaseHandle {
    pub state: *mut c_void,
    pub vtable: *const OutputFollowLeaseVTable,
}
impl OutputFollowLeaseHandle {
    pub const fn null() -> Self {
        Self {
            state: std::ptr::null_mut(),
            vtable: std::ptr::null(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct OutputFollowLeaseVTable {
    pub header: Header,
    /// Synchronously ends the lease: its retention is released and every pending
    /// operation is cancelled or fails closed. Exactly once, on any thread.
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    /// Starts one `next` operation. Input is an encoded acknowledgement of the
    /// last item the holder observed; an unacknowledged item is retained and
    /// resent, so cancelling an operation that raced completion never loses it.
    /// A lease allows one active `next`; another reports BUSY.
    pub next:
        Option<unsafe extern "C" fn(*mut c_void, BorrowedBytes, *mut OperationHandle) -> Status>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_extension_preserves_base_and_read_layouts() {
        let pointer = size_of::<usize>();
        assert_eq!(size_of::<PluginOutputFollowV1>(), 24 + pointer);
        assert_eq!(size_of::<OutputFollowProviderHandle>(), 2 * pointer);
        assert_eq!(size_of::<OutputFollowProviderVTable>(), 16 + 2 * pointer);
        assert_eq!(size_of::<OutputFollowLeaseHandle>(), 2 * pointer);
        assert_eq!(size_of::<OutputFollowLeaseVTable>(), 16 + 2 * pointer);
        assert_eq!(
            size_of::<crate::output_reads::PluginOutputReadsV1>(),
            24 + pointer
        );
        assert_eq!(size_of::<ComponentVTable>(), 16 + 5 * pointer);
        assert_eq!(ABI_VERSION, "1.0.0");
        assert_eq!(WIRE_VERSION, 1);
    }
}
