// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional service contract, versioned independently of the frozen base tables.
//! Absence means no services, not a legacy-family fallback. Validate the header
//! and version before invoking callbacks. All tables obey base ABI ownership.

use super::*;

pub const SYMBOL: &[u8] = b"drasi_computation_plugin_services_v1\0";
pub const VERSION: u32 = 1;
pub type ServicesFn = unsafe extern "C" fn() -> *const PluginServicesV1;

/// Borrows existing plugin/factory handles from this same binary. It neither
/// owns nor releases them. Factory discovery returns bounded MessagePack.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginServicesV1 {
    pub header: Header,
    pub version: u32,
    pub reserved: u32,
    pub factory: Option<unsafe extern "C" fn(*mut c_void, u32) -> Reply>,
    pub create: Option<
        unsafe extern "C" fn(
            *mut c_void,
            BorrowedBytes,
            *const SourceAdmissionV1,
            *mut ComponentHandle,
        ) -> Status,
    >,
}

/// Host-owned, revocable admission capability. A plugin retaining it copies the
/// table and calls retain before returning. Only host callbacks touch context.
/// There is no unrestricted storage, transaction or commit interface.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SourceAdmissionV1 {
    pub header: Header,
    pub context: *mut c_void,
    pub retain: Option<unsafe extern "C" fn(*mut c_void)>,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    pub request: Option<
        unsafe extern "C" fn(*mut c_void, u32, BorrowedBytes, *mut OperationHandle) -> Status,
    >,
    /// First fatal listener/worker failure, as 1..=4096 UTF-8 bytes. Synchronous,
    /// bounded and independent of request capacity; never performs storage I/O.
    pub report_failure: Option<unsafe extern "C" fn(*mut c_void, BorrowedBytes) -> Status>,
    /// Immutable GraphProducerIdentity as bounded MessagePack; no storage I/O.
    pub describe: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
}

pub mod admission {
    pub const REGISTER: u32 = 1;
    pub const RETIRE: u32 = 2;
    pub const STATUS: u32 = 3;
    pub const RECEIPT: u32 = 4;
    pub const ADMIT: u32 = 5;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_service_tables_do_not_extend_the_base_contract() {
        let pointer = size_of::<usize>();
        assert_eq!(size_of::<PluginServicesV1>(), 24 + 2 * pointer);
        assert_eq!(size_of::<SourceAdmissionV1>(), 16 + 6 * pointer);
        assert_eq!(size_of::<PluginVTable>(), 16 + 3 * pointer);
        assert_eq!(size_of::<FactoryVTable>(), 16 + 2 * pointer);
        assert_eq!(size_of::<ComponentVTable>(), 16 + 5 * pointer);
        assert_eq!(ABI_VERSION, "1.0.0");
        assert_eq!(WIRE_VERSION, 2);
    }
}
