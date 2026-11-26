// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional per-component management: assessment and application of a proposed
//! configuration change, separate from the lifecycle operations.
//!
//! `assess` is synchronous and side-effect free. It takes the component's busy
//! slot, so it fails BUSY while another data or lifecycle operation is active.
//! Input is a JSON UpdateRequest; success returns a JSON Assessment, each bounded
//! by MAX_METADATA_BYTES. A component without instance management fails
//! UNSUPPORTED; the host then uses the factory planner.
//!
//! `apply` begins an operation on the ordinary operation transport. Input is a
//! bounded named MessagePack ApplyRequest. It is valid only at the accepted Pause
//! or Stop boundary; Replace is executed by the host through DISPOSE and factory
//! construction. A failed apply reports `{"apply": "refused" | "failed" |
//! "damaged", "nowRequires"?: boundary}` in its Failure data.

use crate::*;

pub const VERSION: u32 = 1;
pub const SYMBOL: &[u8] = b"drasi_computation_plugin_management_v1\0";
pub type ManagementFn = unsafe extern "C" fn() -> *const PluginManagementV1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginManagementV1 {
    pub header: Header,
    pub version: u32,
    pub reserved: u32,
    /// Given ComponentHandle.state from this binary.
    pub assess: Option<unsafe extern "C" fn(*mut c_void, BorrowedBytes) -> Reply>,
    /// Given ComponentHandle.state from this binary. On success, out owns one
    /// operation handle; on failure it remains null.
    pub apply:
        Option<unsafe extern "C" fn(*mut c_void, BorrowedBytes, *mut OperationHandle) -> Status>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn management_is_an_extension_of_the_base_abi() {
        let pointer = size_of::<usize>();
        assert_eq!(size_of::<PluginManagementV1>(), 24 + 2 * pointer);
        assert_eq!(size_of::<ComponentVTable>(), 16 + 5 * pointer);
        assert_eq!(ABI_VERSION, "0.2.0");
        assert_eq!(WIRE_VERSION, 1);
    }
}
