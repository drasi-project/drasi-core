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
//! All types in this module are `#[repr(C)]` and safe to pass across
//! separately-compiled cdylib boundaries via the stable C ABI.

use std::ffi::c_void;
use std::os::raw::c_char;

// ============================================================================
// Executor: runs a future on the originating cdylib's tokio runtime
// ============================================================================

/// Type-erased async executor. Each cdylib provides one that dispatches
/// futures onto its own tokio runtime.
pub type AsyncExecutorFn = extern "C" fn(future_ptr: *mut c_void) -> *mut c_void;

// ============================================================================
// Strings
// ============================================================================

/// A borrowed string that can cross the FFI boundary.
/// The data is owned by whoever created it; the receiver must copy if it needs to keep it.
#[repr(C)]
pub struct FfiStr {
    pub ptr: *const c_char,
    pub len: usize,
}

impl FfiStr {
    pub fn from_str(s: &str) -> Self {
        Self {
            ptr: s.as_ptr() as *const c_char,
            len: s.len(),
        }
    }

    /// # Safety
    /// The pointer must be valid for `len` bytes and contain valid UTF-8.
    pub unsafe fn as_str(&self) -> &str {
        let bytes = std::slice::from_raw_parts(self.ptr as *const u8, self.len);
        std::str::from_utf8_unchecked(bytes)
    }

    /// # Safety
    /// The pointer must be valid for `len` bytes and contain valid UTF-8.
    pub unsafe fn to_string(&self) -> String {
        self.as_str().to_owned()
    }
}

// FfiStr is safe to share when pointing to static string data or data owned by the sender.
unsafe impl Send for FfiStr {}
unsafe impl Sync for FfiStr {}

/// Owned FFI string — the data is heap-allocated and must be freed by the creator.
#[repr(C)]
pub struct FfiOwnedStr {
    pub ptr: *mut c_char,
    pub len: usize,
    pub cap: usize,
}

impl FfiOwnedStr {
    pub fn from_string(s: String) -> Self {
        let mut bytes = s.into_bytes();
        let result = Self {
            ptr: bytes.as_mut_ptr() as *mut c_char,
            len: bytes.len(),
            cap: bytes.capacity(),
        };
        std::mem::forget(bytes);
        result
    }

    /// # Safety
    /// Must only be called once; the pointer must have been created by `from_string`.
    pub unsafe fn into_string(self) -> String {
        String::from_raw_parts(self.ptr as *mut u8, self.len, self.cap)
    }

    pub fn as_ffi_str(&self) -> FfiStr {
        FfiStr {
            ptr: self.ptr as *const c_char,
            len: self.len,
        }
    }
}

/// FFI-safe array of owned strings.
#[repr(C)]
pub struct FfiStringArray {
    pub data: *mut FfiOwnedStr,
    pub len: usize,
    pub cap: usize,
}

impl FfiStringArray {
    pub fn from_vec(v: Vec<String>) -> Self {
        let mut owned: Vec<FfiOwnedStr> = v.into_iter().map(FfiOwnedStr::from_string).collect();
        let result = Self {
            data: owned.as_mut_ptr(),
            len: owned.len(),
            cap: owned.capacity(),
        };
        std::mem::forget(owned);
        result
    }

    /// # Safety
    /// Must only be called once. Consumes all strings and the backing array.
    pub unsafe fn into_vec(self) -> Vec<String> {
        let owned = Vec::from_raw_parts(self.data, self.len, self.cap);
        owned.into_iter().map(|s| s.into_string()).collect()
    }
}

// ============================================================================
// Result
// ============================================================================

/// Result of an operation across the FFI boundary.
#[repr(C)]
pub struct FfiResult {
    pub error_code: i32,
    pub error_msg: *mut c_char,
}

impl FfiResult {
    pub fn ok() -> Self {
        Self {
            error_code: 0,
            error_msg: std::ptr::null_mut(),
        }
    }

    pub fn err(msg: String) -> Self {
        let c_msg = std::ffi::CString::new(msg).unwrap_or_default();
        Self {
            error_code: 1,
            error_msg: c_msg.into_raw(),
        }
    }

    /// Wrap a panic payload into an FfiResult error.
    pub fn from_panic(payload: Box<dyn std::any::Any + Send>) -> Self {
        let msg = if let Some(s) = payload.downcast_ref::<&str>() {
            format!("plugin panic: {}", s)
        } else if let Some(s) = payload.downcast_ref::<String>() {
            format!("plugin panic: {}", s)
        } else {
            "plugin panic: <unknown>".to_string()
        };
        Self::err(msg)
    }

    /// # Safety
    /// Must only be called once if error_msg is non-null.
    pub unsafe fn into_result(self) -> Result<(), String> {
        if self.error_code == 0 {
            Ok(())
        } else if !self.error_msg.is_null() {
            let msg = std::ffi::CString::from_raw(self.error_msg)
                .into_string()
                .unwrap_or_default();
            Err(msg)
        } else {
            Err("unknown error".to_string())
        }
    }
}

// Safety: FfiResult owns its error_msg pointer; it's only consumed once via into_result().
unsafe impl Send for FfiResult {}

// ============================================================================
// Enums
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

// ============================================================================
// State store get result
// ============================================================================

/// Result of a state store get operation.
#[repr(C)]
pub struct FfiGetResult {
    pub data: *mut u8,
    pub len: usize,
    pub cap: usize,
    pub found: bool,
    pub error_code: i32,
    pub error_msg: *mut c_char,
}

impl FfiGetResult {
    pub fn not_found() -> Self {
        Self {
            data: std::ptr::null_mut(),
            len: 0,
            cap: 0,
            found: false,
            error_code: 0,
            error_msg: std::ptr::null_mut(),
        }
    }

    pub fn found(data: Vec<u8>) -> Self {
        let mut data = data;
        let result = Self {
            data: data.as_mut_ptr(),
            len: data.len(),
            cap: data.capacity(),
            found: true,
            error_code: 0,
            error_msg: std::ptr::null_mut(),
        };
        std::mem::forget(data);
        result
    }

    pub fn err(msg: String) -> Self {
        let c_msg = std::ffi::CString::new(msg).unwrap_or_default();
        Self {
            data: std::ptr::null_mut(),
            len: 0,
            cap: 0,
            found: false,
            error_code: 1,
            error_msg: c_msg.into_raw(),
        }
    }

    /// # Safety
    /// Must only be called once. Consumes the data allocation.
    pub unsafe fn into_result(self) -> Result<Option<Vec<u8>>, String> {
        if self.error_code != 0 {
            if !self.error_msg.is_null() {
                return Err(std::ffi::CString::from_raw(self.error_msg)
                    .into_string()
                    .unwrap_or_default());
            }
            return Err("state store error".into());
        }
        if !self.found {
            return Ok(None);
        }
        Ok(Some(Vec::from_raw_parts(self.data, self.len, self.cap)))
    }
}

// ============================================================================
// Utilities
// ============================================================================

/// Current timestamp in microseconds since UNIX epoch.
pub fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64
}

/// Wrap a closure in `catch_unwind`, converting panics to `FfiResult::err`.
pub fn catch_panic_ffi<F: FnOnce() -> FfiResult + std::panic::UnwindSafe>(f: F) -> FfiResult {
    match std::panic::catch_unwind(f) {
        Ok(result) => result,
        Err(payload) => FfiResult::from_panic(payload),
    }
}

/// Wrapper to make a raw pointer Send-safe for thread::spawn.
///
/// # Safety
/// The caller must ensure the pointed-to data lives long enough.
pub struct SendPtr<T>(pub *const T);
unsafe impl<T> Send for SendPtr<T> {}
impl<T> SendPtr<T> {
    /// # Safety
    /// The pointer must be valid and point to a live value.
    pub unsafe fn as_ref(&self) -> &T {
        &*self.0
    }
}

/// Mutable version of `SendPtr`.
pub struct SendMutPtr<T>(pub *mut T);
unsafe impl<T> Send for SendMutPtr<T> {}
impl<T> SendMutPtr<T> {
    pub fn as_ptr(&self) -> *mut T {
        self.0
    }
}
