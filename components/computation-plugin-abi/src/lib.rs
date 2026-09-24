// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at http://www.apache.org/licenses/LICENSE-2.0
// Unless required by applicable law or agreed to in writing, software distributed
// under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
// CONDITIONS OF ANY KIND, either express or implied.

//! Native ComputationGraph ABI, deliberately unrelated to Source/Reaction ABI 0.15.
//!
//! Every pointer is either borrowed for the duration of a call or an opaque,
//! producer-owned handle. Only the producer may dereference or release a handle.
//! No Rust allocator, future, trait object, task-local, or container ABI is shared.
//! All entry points and callbacks must contain panics. Libraries are pinned for
//! process lifetime; this ABI does not provide hot unloading.
//!
//! V1 headers and layouts are frozen. Read and validate the header before reading
//! the rest of a table. V1 requires an exact version and size, including reserved
//! fields. Metadata and tables remain valid until their owning handle is released.
//! Calls on one operation are serialized by its owner; wake/retain/release may be
//! called concurrently. A poll must be cooperative and must never wait for I/O.

use std::{ffi::c_void, fmt, mem::size_of, ptr};

pub const ABI_VERSION: &str = "1.0.0";
pub const ABI_MAJOR: u16 = 1;
pub const ABI_MINOR: u16 = 0;
pub const ABI_MAGIC: u64 = 0x4452_4153_4943_4731; // DRASICG1
pub const WIRE_VERSION: u32 = 2;
pub const METADATA_SYMBOL: &[u8] = b"drasi_computation_plugin_metadata\0";
pub const ENTRY_SYMBOL: &[u8] = b"drasi_computation_plugin_entry\0";
pub const MAX_METADATA_BYTES: usize = 1024 * 1024;
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub magic: u64,
    pub major: u16,
    pub minor: u16,
    pub size: u32,
}

impl Header {
    pub const fn new<T>() -> Self {
        Self {
            magic: ABI_MAGIC,
            major: ABI_MAJOR,
            minor: ABI_MINOR,
            size: size_of::<T>() as u32,
        }
    }

    pub fn validate<T>(&self) -> Result<(), InvalidHeader> {
        if *self == Self::new::<T>() {
            Ok(())
        } else {
            Err(InvalidHeader(*self))
        }
    }
}

#[derive(Debug)]
pub struct InvalidHeader(pub Header);

impl fmt::Display for InvalidHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "incompatible native computation ABI header: {:?}",
            self.0
        )
    }
}
impl std::error::Error for InvalidHeader {}

/// Borrowed bytes. Null is allowed only for length zero. The callee must copy
/// before returning if it needs the bytes later, including in an operation.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BorrowedBytes {
    pub data: *const u8,
    pub len: usize,
}
impl BorrowedBytes {
    pub const fn empty() -> Self {
        Self {
            data: ptr::null(),
            len: 0,
        }
    }
    pub fn new(bytes: &[u8]) -> Self {
        Self {
            data: bytes.as_ptr(),
            len: bytes.len(),
        }
    }
}

/// Ownership is transferred, not shared. Invoke `release(context)` exactly once,
/// even for an error or an unread payload. Never free `data` on the receiving side.
/// The canonical empty buffer has null data/context and no release function.
/// Nonempty data remains immutable and valid until release. The receiver may
/// borrow it for decoding and move ownership between threads; release must be
/// callable on any thread, independently of the originating operation's lifetime.
#[repr(C)]
pub struct OwnedBytes {
    pub data: *const u8,
    pub len: usize,
    pub context: *mut c_void,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
}
impl OwnedBytes {
    pub const fn empty() -> Self {
        Self {
            data: ptr::null(),
            len: 0,
            context: ptr::null_mut(),
            release: None,
        }
    }
}

pub mod status {
    pub const OK: u32 = 0;
    pub const INVALID_ARGUMENT: u32 = 1;
    pub const UNSUPPORTED: u32 = 2;
    pub const FAILED: u32 = 3;
    pub const PANICKED: u32 = 4;
    pub const CANCELLED: u32 = 5;
    pub const CLOSED: u32 = 6;
    pub const BUSY: u32 = 7;
    pub const PROTOCOL: u32 = 8;
}

/// Nonzero codes carry UTF-8 JSON Failure data (code, message, retryable). Success
/// must have an empty error buffer. An unrecognized code is a failure, never success.
#[repr(C)]
pub struct Status {
    pub code: u32,
    pub error: OwnedBytes,
}
impl Status {
    pub const fn ok() -> Self {
        Self {
            code: status::OK,
            error: OwnedBytes::empty(),
        }
    }
}

#[repr(C)]
pub struct Reply {
    pub status: Status,
    pub payload: OwnedBytes,
}
impl Reply {
    pub const fn empty() -> Self {
        Self {
            status: Status::ok(),
            payload: OwnedBytes::empty(),
        }
    }
}

/// Both byte strings are immutable for the lifetime of the loaded library.
/// Manifest is required UTF-8 JSON PluginMetadata with wire_version == WIRE_VERSION.
#[repr(C)]
pub struct Metadata {
    pub header: Header,
    pub target: BorrowedBytes,
    pub manifest: BorrowedBytes,
}

macro_rules! handle {
    ($name:ident, $table:ident) => {
        #[repr(C)]
        pub struct $name {
            pub state: *mut c_void,
            pub vtable: *const $table,
        }
        impl $name {
            pub const fn null() -> Self {
                Self {
                    state: ptr::null_mut(),
                    vtable: ptr::null(),
                }
            }
        }
    };
}
handle!(PluginHandle, PluginVTable);
handle!(FactoryHandle, FactoryVTable);
handle!(ComponentHandle, ComponentVTable);
handle!(OperationHandle, OperationVTable);

pub type MetadataFn = unsafe extern "C" fn() -> *const Metadata;
/// On failure, out must remain null. On success, out owns one reference.
pub type EntryFn = unsafe extern "C" fn(out: *mut PluginHandle) -> Status;

#[repr(C)]
pub struct PluginVTable {
    pub header: Header,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    pub factory: Option<unsafe extern "C" fn(*mut c_void, u32, *mut FactoryHandle) -> Status>,
    /// Pure synchronous record validation against a manifest schema.
    pub validate_record: Option<unsafe extern "C" fn(*mut c_void, BorrowedBytes) -> Status>,
}

#[repr(C)]
pub struct FactoryVTable {
    pub header: Header,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    /// Configuration-only constructor. No I/O, activation, workers or blocking.
    pub create:
        Option<unsafe extern "C" fn(*mut c_void, BorrowedBytes, *mut ComponentHandle) -> Status>,
}

#[repr(C)]
pub struct ComponentVTable {
    pub header: Header,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    pub inspect: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
    pub configuration: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
    /// Borrowed callback table. Retain it before storing; release old bindings.
    pub bind_control: Option<unsafe extern "C" fn(*mut c_void, *const Control) -> Status>,
    /// Copy input and retain optional transaction before returning. Data/lifecycle
    /// calls are serialized. Control and wakeup calls use independent shared state.
    pub begin: Option<
        unsafe extern "C" fn(
            *mut c_void,
            u32,
            BorrowedBytes,
            *const Transaction,
            *mut OperationHandle,
        ) -> Status,
    >,
}

pub mod operation {
    pub const START: u32 = 1;
    pub const STOP: u32 = 2;
    pub const NEXT: u32 = 3;
    pub const TRANSFORM: u32 = 4;
    pub const HANDLE: u32 = 5;
    pub const RUN: u32 = 6;
    pub const QUIESCE: u32 = 7;
    pub const DELIVERY_COMPLETED: u32 = 8;
    pub const CONTINUE: u32 = 9;
    pub const WAKE_WAIT: u32 = 10;
    pub const WAKE_PENDING: u32 = 11;
    pub const ON_WAKEUP: u32 = 12;
    pub const SNAPSHOT: u32 = 13;
    pub const CONTROL: u32 = 14;
    pub const TRANSACT: u32 = 15;
}

pub const POLL_PENDING: u32 = 0;
pub const POLL_READY: u32 = 1;

#[repr(C)]
pub struct PollResult {
    pub state: u32,
    pub reply: Reply,
}

#[repr(C)]
pub struct OperationVTable {
    pub header: Header,
    /// Borrow wake for this call; retain any copy stored in a future/I/O resource.
    /// READY transfers its reply once. Further polls must report an error.
    pub poll: Option<unsafe extern "C" fn(*mut c_void, *const Wake) -> PollResult>,
    /// Revoke and drop pending work, without waiting for I/O. Subsequent polls of a
    /// cancelled operation report CANCELLED. Cancellation is not rollback.
    pub cancel: Option<unsafe extern "C" fn(*mut c_void) -> Status>,
    /// Cancel pending work and release the owner's reference.
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
}

/// Refcount operations run on the producing side only. A retained wake can
/// outlive cancellation/release; its context must remain callable until released.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Wake {
    pub header: Header,
    pub context: *mut c_void,
    pub retain: Option<unsafe extern "C" fn(*mut c_void)>,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    pub wake: Option<unsafe extern "C" fn(*mut c_void)>,
}

/// Dedicated, nonblocking control plane. Sending never waits for data capacity.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Control {
    pub header: Header,
    pub context: *mut c_void,
    pub retain: Option<unsafe extern "C" fn(*mut c_void)>,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    pub send: Option<unsafe extern "C" fn(*mut c_void, BorrowedBytes) -> Status>,
}

/// Revocable step-scoped state capability. The context never contains a borrowed
/// Rust TransactionContext. Host-owned requests are polled inside that borrow's
/// scope; after completion/cancellation every new request must fail CLOSED.
/// There is deliberately no commit, storage reconfiguration or cross-step access.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Transaction {
    pub header: Header,
    pub context: *mut c_void,
    pub retain: Option<unsafe extern "C" fn(*mut c_void)>,
    pub release: Option<unsafe extern "C" fn(*mut c_void)>,
    pub request: Option<
        unsafe extern "C" fn(*mut c_void, u32, BorrowedBytes, *mut OperationHandle) -> Status,
    >,
}

pub mod transaction {
    pub const GET: u32 = 1;
    pub const PUT: u32 = 2;
    pub const REMOVE: u32 = 3;
    pub const GET_ELEMENT: u32 = 4;
    pub const PUT_ELEMENT: u32 = 5;
    pub const REMOVE_ELEMENT: u32 = 6;
    pub const DERIVE: u32 = 7;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of};

    #[test]
    fn frozen_headers_and_pointer_layouts() {
        assert_eq!(size_of::<Header>(), 16);
        assert_eq!(offset_of!(Header, magic), 0);
        assert_eq!(offset_of!(Header, major), 8);
        assert_eq!(offset_of!(Header, minor), 10);
        assert_eq!(offset_of!(Header, size), 12);
        let p = size_of::<usize>();
        assert_eq!(size_of::<BorrowedBytes>(), 2 * p);
        assert_eq!(size_of::<OwnedBytes>(), 4 * p);
        assert_eq!(offset_of!(OwnedBytes, data), 0);
        assert_eq!(offset_of!(OwnedBytes, len), p);
        assert_eq!(offset_of!(OwnedBytes, context), 2 * p);
        assert_eq!(offset_of!(OwnedBytes, release), 3 * p);
        assert_eq!(size_of::<OperationHandle>(), 2 * p);
        assert_eq!(offset_of!(OperationHandle, state), 0);
        assert_eq!(offset_of!(OperationHandle, vtable), p);
        assert_eq!(offset_of!(Metadata, header), 0);
        assert_eq!(offset_of!(Metadata, target), 16);
        assert_eq!(size_of::<Metadata>(), 16 + 4 * p);
        assert_eq!(size_of::<PluginVTable>(), 16 + 3 * p);
        assert_eq!(size_of::<FactoryVTable>(), 16 + 2 * p);
        assert_eq!(size_of::<ComponentVTable>(), 16 + 5 * p);
        assert_eq!(size_of::<OperationVTable>(), 16 + 3 * p);
        assert_eq!(size_of::<Wake>(), 16 + 4 * p);
        assert_eq!(size_of::<Control>(), 16 + 4 * p);
        assert_eq!(size_of::<Transaction>(), 16 + 4 * p);
        assert_eq!(offset_of!(OperationVTable, poll), 16);
        assert_eq!(offset_of!(OperationVTable, cancel), 16 + p);
        assert_eq!(offset_of!(OperationVTable, release), 16 + 2 * p);
        assert_eq!(size_of::<Option<MetadataFn>>(), p);
        assert_eq!(size_of::<Option<EntryFn>>(), p);
        assert_eq!(align_of::<OwnedBytes>(), align_of::<usize>());
    }

    #[test]
    fn rejects_other_families_versions_and_truncated_tables() {
        let good = Header::new::<ComponentVTable>();
        good.validate::<ComponentVTable>().unwrap();
        for bad in [
            Header { magic: 0, ..good },
            Header { major: 0, ..good },
            Header { minor: 1, ..good },
            Header { size: 16, ..good },
            Header {
                size: good.size + 8,
                ..good
            },
        ] {
            assert!(bad.validate::<ComponentVTable>().is_err());
        }
    }
}
