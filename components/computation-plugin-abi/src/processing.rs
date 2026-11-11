// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional host-owned processing sessions for native transformers.

use crate::*;

pub const VERSION: u32 = 1;
pub const SYMBOL: &[u8] = b"drasi_computation_plugin_processing_v1\0";
pub type ProcessingFn = unsafe extern "C" fn() -> *const PluginProcessingV1;
pub const MAX_OUTBOX_PAGE_RECORDS: usize = 1024;
pub const MAX_LIVE_RESULT_PAGE_RECORDS: usize = 1024;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginProcessingV1 {
    pub header: Header,
    pub version: u32,
    pub reserved: u32,
    pub create: Option<
        unsafe extern "C" fn(
            *mut c_void,
            BorrowedBytes,
            *const ProcessingV1,
            *mut ComponentHandle,
        ) -> Status,
    >,
}

/// A retained host capability. Each accepted operation is driven within the
/// actual host-owned transaction; the plugin cannot commit it independently.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProcessingV1 {
    pub header: Header,
    pub context: *mut c_void,
    pub retain: Option<unsafe extern "C" fn(*mut c_void)>,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    /// Copies and retains `work` before returning an operation handle.
    pub run: Option<
        unsafe extern "C" fn(*mut c_void, *const ProcessingWorkV1, *mut OperationHandle) -> Status,
    >,
}

/// Negotiated scheduled-processing extension. The base table is unchanged;
/// only factories declaring `scheduled_processing` receive this exact header.
/// Observation is read-only and never opens a transaction or consumes work.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ScheduledProcessingV1 {
    pub processing: ProcessingV1,
    pub version: u32,
    pub reserved: u32,
    pub observe: Option<unsafe extern "C" fn(*mut c_void, u32, *mut OperationHandle) -> Status>,
}

pub const SCHEDULING_VERSION: u32 = 1;

pub mod schedule_observation {
    pub const WAIT: u32 = 1;
    pub const NEXT_DEADLINE: u32 = 2;
}

/// One plugin-owned operation. The host calls `begin` once while its actual
/// transaction is active and revokes the supplied state table when it ends.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ProcessingWorkV1 {
    pub header: Header,
    pub context: *mut c_void,
    pub retain: Option<unsafe extern "C" fn(*mut c_void)>,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    pub begin: Option<
        unsafe extern "C" fn(*mut c_void, *const Transaction, *mut OperationHandle) -> Status,
    >,
}

/// Opt-in output work descriptor. The base work table stays unchanged; callers
/// pass this exact larger header only for the negotiated durable-output profile.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct OutputProcessingWorkV1 {
    pub work: ProcessingWorkV1,
    pub mode: u32,
    pub reserved: u32,
}

pub const OUTPUT_WORK: u32 = 1;
/// Commit evaluation/input state but retain its progress until a later output
/// operation explicitly resumes it. Requires non_atomic_publication.
pub const DEFERRED_PROGRESS_WORK: u32 = 2;

pub mod request {
    pub const GET_ELEMENT: u32 = 1;
    pub const SET_ELEMENT: u32 = 2;
    pub const DELETE_ELEMENT: u32 = 3;
    pub const GET_SLOT_ELEMENT: u32 = 4;
    pub const SET_JOINS: u32 = 5;
    pub const CLEAR_ELEMENTS: u32 = 6;
    pub const GET_ARCHIVE_ELEMENT: u32 = 7;
    pub const CLEAR_ARCHIVE: u32 = 8;
    pub const FUTURE_PUSH: u32 = 9;
    pub const FUTURE_REMOVE: u32 = 10;
    pub const FUTURE_POP: u32 = 11;
    pub const FUTURE_PEEK: u32 = 12;
    pub const FUTURE_CLEAR: u32 = 13;
    pub const RESULT_GET: u32 = 14;
    pub const RESULT_SET: u32 = 15;
    pub const SLOT_OPEN: u32 = 16;
    pub const SLOT_INBOUND_OPEN: u32 = 17;
    pub const SLOT_OUTBOUND_OPEN: u32 = 18;
    pub const SLOT_NEXT: u32 = 19;
    pub const SORTED_GET_NEXT: u32 = 20;
    pub const SORTED_GET_COUNT: u32 = 21;
    pub const SORTED_INCREMENT: u32 = 22;
    pub const ATOMIC_STATE_INFO: u32 = 23;
    pub const CHECKPOINT_GET: u32 = 24;
    pub const CHECKPOINT_STAGE: u32 = 25;
    pub const RESULT_SEQUENCE_GET: u32 = 26;
    pub const RESULT_SEQUENCE_STAGE: u32 = 27;
    pub const OUTBOX_READ_PAGE: u32 = 28;
    pub const OUTBOX_APPEND_AND_TRIM: u32 = 29;
    pub const LIVE_ROWS_READ_PAGE: u32 = 30;
    pub const LIVE_ROWS_APPLY: u32 = 31;
    pub const INPUT_CHECKPOINT_GET: u32 = 32;
    pub const INPUT_CHECKPOINT_STAGE: u32 = 33;
    pub const INPUT_PROGRESS_RECOVER: u32 = 34;
    pub const BOOTSTRAP_COMPLETE_STAGE: u32 = 35;
    pub const INPUT_TRANSPORT_STAGE: u32 = 36;
    pub const ATOMIC_STATE_RESET: u32 = 37;
    pub const RESET_GENERATION_GET: u32 = 38;
    pub const DELIVERY_PROGRESS_GET: u32 = 39;
    pub const DELIVERY_PROGRESS_STAGE: u32 = 40;
    /// Open an archive range as a bounded element stream consumed with SLOT_NEXT.
    pub const ARCHIVE_VERSIONS_OPEN: u32 = 41;
    pub const INPUT_PROGRESS_PREPARE: u32 = 42;
    pub const INPUT_PROGRESS_STAGE: u32 = 43;
    pub const INPUT_PRODUCER_BIND: u32 = 44;
    pub const PHYSICAL_TIME: u32 = 45;
    pub const STATE_INFO: u32 = 46;
    pub const VOLATILE_STATE_RESET: u32 = 47;
    pub const NEXT_EMISSION_SEQUENCE: u32 = 48;
    pub const INPUT_PROGRESS_RESUME: u32 = 49;
    pub const NON_ATOMIC_RESET_BEGIN: u32 = 50;
    pub const NON_ATOMIC_STATE_RESET: u32 = 51;
    pub const NON_ATOMIC_RESET_FINISH: u32 = 52;
    pub const INPUT_PROGRESS_VALIDATE: u32 = 53;
    /// Restore an output baseline inside the operation that performed an owned
    /// bootstrap reset. This is not delivery acknowledgement.
    pub const RESET_OUTPUT_BASELINE: u32 = 54;
    /// Component-local scheduled work. These requests require the explicitly
    /// negotiated ScheduledProcessingV1 service and its atomic storage profile.
    pub const SCHEDULE_REQUEST: u32 = 55;
    pub const SCHEDULE_REPLACE: u32 = 56;
    pub const SCHEDULE_CANCEL: u32 = 57;
    pub const SCHEDULE_GET: u32 = 58;
    pub const SCHEDULE_CLAIM_DUE: u32 = 59;
    pub const SCHEDULE_COMPLETE: u32 = 60;
}

/// Processing-specific failure codes. Messages for errors carrying a string
/// contain `:` followed by the original store name or provider error text.
pub mod index_error {
    pub const IO: u32 = 1024;
    pub const NOT_SUPPORTED: u32 = 1025;
    pub const ARCHIVE_NOT_ENABLED: u32 = 1026;
    pub const CORRUPTED_DATA: u32 = 1027;
    pub const CONNECTION_FAILED: u32 = 1028;
    pub const UNKNOWN_STORE: u32 = 1029;
    pub const OTHER: u32 = 1030;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processing_extension_does_not_change_base_layouts() {
        let pointer = size_of::<usize>();
        assert_eq!(size_of::<PluginProcessingV1>(), 24 + pointer);
        assert_eq!(size_of::<ProcessingV1>(), 16 + 4 * pointer);
        assert_eq!(size_of::<ScheduledProcessingV1>(), 24 + 5 * pointer);
        assert_eq!(size_of::<ProcessingWorkV1>(), 16 + 4 * pointer);
        assert_eq!(size_of::<OutputProcessingWorkV1>(), 24 + 4 * pointer);
        assert_eq!(size_of::<ComponentVTable>(), 16 + 5 * pointer);
        assert_eq!(ABI_VERSION, "2.0.0");
    }
}
