// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Core FFI-safe types for the cdylib plugin boundary.
//!
//! All types are re-exported from [`drasi_ffi_primitives`]. This module
//! exists for backward compatibility — consumers should prefer importing
//! from `drasi_ffi_primitives` directly where possible.

pub use drasi_ffi_primitives::{
    catch_panic_ffi, now_us, AsyncExecutorFn, FfiGetResult, FfiOwnedStr, FfiResult, FfiStr,
    FfiStringArray, SendMutPtr, SendPtr,
};

// ============================================================================
// Enums — domain-specific, remain here
// ============================================================================

/// Component status, FFI-safe.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfiComponentStatus {
    Stopped = 0,
    Starting = 1,
    Running = 2,
    Stopping = 3,
    Reconfiguring = 4,
    Error = 5,
}

/// Change operation type, FFI-safe.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfiChangeOp {
    Insert = 0,
    Update = 1,
    Delete = 2,
}

/// Dispatch mode, FFI-safe.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfiDispatchMode {
    Broadcast = 0,
    Channel = 1,
}
