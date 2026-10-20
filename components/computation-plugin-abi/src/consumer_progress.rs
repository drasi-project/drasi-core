// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional read-only progress for a producer's immutable consumer set. This
//! extension changes neither base ABI 1.0 nor the single-owner recovery-v1 ABI.

use super::*;

pub const SYMBOL: &[u8] = b"drasi_computation_plugin_consumer_progress_v1\0";
pub const VERSION: u32 = 1;
pub const MAX_PROGRESS_MEMBERS: usize = 256;
/// Limits apply to the complete set, not separately to each member.
pub const MAX_PROGRESS_BYTES: usize = 1024 * 1024;
pub const MAX_PROGRESS_ENTRIES: usize = 4096;
pub const MAX_SUBSCRIPTIONS: usize = 16;
pub const MAX_POSITION_BYTES: usize = 65_536;
pub type ConsumerProgressFn = unsafe extern "C" fn() -> *const PluginConsumerProgressV1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginConsumerProgressV1 {
    pub header: Header,
    pub version: u32,
    pub reserved: u32,
    pub factory: Option<unsafe extern "C" fn(*mut c_void, u32) -> Reply>,
    /// An advertising factory requires this binding; the base create path fails.
    pub create: Option<
        unsafe extern "C" fn(
            *mut c_void,
            BorrowedBytes,
            *const ConsumerProgressV1,
            *mut ComponentHandle,
        ) -> Status,
    >,
    /// Immutable upstream retention declaration, not consumer durability proof.
    pub inspect: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
    /// Borrowed immutable policy, or a null table when none was declared.
    pub position_policy:
        Option<unsafe extern "C" fn(*mut c_void, *mut InputPositionPolicyV1) -> Status>,
}

/// Producer-owned immutable comparator, captured at construction. Retain before
/// storing; it stays callable until release without borrowing mutable component
/// state, including during a pending data operation. Graph installation and
/// revocation remain host-owned. Comparisons perform no I/O and return a bounded
/// MessagePack boolean; invalid/incompatible positions and panics remain errors.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InputPositionPolicyV1 {
    pub header: Header,
    pub context: *mut c_void,
    pub retain: Option<unsafe extern "C" fn(*mut c_void)>,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    pub describe: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
    pub is_after: Option<unsafe extern "C" fn(*mut c_void, BorrowedBytes, BorrowedBytes) -> Reply>,
}

impl InputPositionPolicyV1 {
    pub const fn null() -> Self {
        Self {
            header: Header::new::<Self>(),
            context: ptr::null_mut(),
            retain: None,
            release: None,
            describe: None,
            is_after: None,
        }
    }
}

/// Borrowed, retained host capability. Describe identifies the graph, producer
/// and complete immutable membership. Reads perform no I/O and never aggregate
/// checkpoints, opaque positions, durability or failure state.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ConsumerProgressV1 {
    pub header: Header,
    pub context: *mut c_void,
    pub retain: Option<unsafe extern "C" fn(*mut c_void)>,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    pub describe: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
    pub snapshot: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
    pub subscribe:
        Option<unsafe extern "C" fn(*mut c_void, *mut ConsumerProgressSubscriptionV1) -> Status>,
}

/// Owned subscription, transferred on successful subscribe and released once.
/// Only successful snapshots mark the returned versions observed. A wait returns
/// empty success on a change, without consuming it, and cancellation is safe.
/// One wait may be pending per subscription. Stop/revocation wakes pending waits
/// and fences old subscriptions permanently, including across a subsequent start.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ConsumerProgressSubscriptionV1 {
    pub header: Header,
    pub context: *mut c_void,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    pub snapshot: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
    pub wait: Option<unsafe extern "C" fn(*mut c_void, *mut OperationHandle) -> Status>,
}

impl ConsumerProgressSubscriptionV1 {
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
    fn consumer_progress_preserves_base_and_single_owner_layouts() {
        let pointer = size_of::<usize>();
        assert_eq!(size_of::<PluginConsumerProgressV1>(), 24 + 4 * pointer);
        assert_eq!(size_of::<InputPositionPolicyV1>(), 16 + 5 * pointer);
        assert_eq!(size_of::<ConsumerProgressV1>(), 16 + 6 * pointer);
        assert_eq!(
            size_of::<ConsumerProgressSubscriptionV1>(),
            16 + 4 * pointer
        );
        assert_eq!(size_of::<recovery::PluginRecoveryV1>(), 24 + 3 * pointer);
        assert_eq!(size_of::<recovery::SourceProgressV1>(), 16 + 6 * pointer);
        assert_eq!(
            size_of::<recovery::ProgressSubscriptionV1>(),
            16 + 4 * pointer
        );
        assert_eq!(size_of::<ComponentVTable>(), 16 + 5 * pointer);
        assert_eq!(ABI_VERSION, "1.0.0");
        assert_eq!(WIRE_VERSION, 2);
        assert!(ConsumerProgressSubscriptionV1::null().context.is_null());
    }
}
