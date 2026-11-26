// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Host services bound to one construction. A factory declares the services it
//! is constructed with once, in its manifest capabilities; the Host binds any
//! subset of them together and passes them through the single
//! [`FactoryVTable::create`] entry. All tables obey base ABI ownership.

use super::*;

/// Every Host service bound to one construction. A null pointer (or zero
/// `consumer`) means that service is not bound for this construction. The
/// Host binds only services the factory's manifest declares; the producer
/// refuses an undeclared binding and a missing required one (consumer
/// progress, indexed processing, consumer delivery). Each pointer is borrowed
/// for the duration of the create call; the producer retains what it keeps.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ServiceBindingsV1 {
    pub header: Header,
    pub admission: *const SourceAdmissionV1,
    pub source_progress: *const recovery::SourceProgressV1,
    pub consumer_progress: *const consumer_progress::ConsumerProgressV1,
    pub output_snapshots: *const output_snapshots::OutputSnapshotsV1,
    /// `ProcessingV1`, or the `ScheduledProcessingV1` prefix when the factory
    /// declares scheduled processing.
    pub processing: *const processing::ProcessingV1,
    /// The source's retained bootstrap coordinator, created through
    /// `PluginBootstrapCoordinationV1::create` from the same progress owners.
    pub coordinator: *const bootstrap_coordination::BootstrapCoordinatorHandle,
    /// Nonzero when the Host-owned consumer delivery service is bound.
    pub consumer: u32,
    pub reserved: u32,
}

impl ServiceBindingsV1 {
    /// No bound services.
    pub const fn none() -> Self {
        Self {
            header: Header::new::<Self>(),
            admission: ptr::null(),
            source_progress: ptr::null(),
            consumer_progress: ptr::null(),
            output_snapshots: ptr::null(),
            processing: ptr::null(),
            coordinator: ptr::null(),
            consumer: 0,
            reserved: 0,
        }
    }
}

/// Host-owned source admission. An ADMIT request carries `wait`
/// (back-pressure instead of `Busy`/`CapacityExhausted`).
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
    fn one_binding_table_carries_every_service() {
        let pointer = size_of::<usize>();
        assert_eq!(size_of::<ServiceBindingsV1>(), 24 + 6 * pointer);
        assert_eq!(size_of::<SourceAdmissionV1>(), 16 + 6 * pointer);
        assert_eq!(size_of::<PluginVTable>(), 16 + 3 * pointer);
        assert_eq!(size_of::<FactoryVTable>(), 16 + 4 * pointer);
        assert_eq!(size_of::<ComponentVTable>(), 16 + 5 * pointer);
        let none = ServiceBindingsV1::none();
        none.header.validate::<ServiceBindingsV1>().unwrap();
        assert!(none.admission.is_null() && none.processing.is_null() && none.consumer == 0);
        assert_eq!(ABI_VERSION, "0.2.0");
        assert_eq!(WIRE_VERSION, 1);
    }
}
