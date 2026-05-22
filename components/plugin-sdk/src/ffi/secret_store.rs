// Copyright 2026 The Drasi Authors.
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

//! FFI-safe SecretStoreProvider vtable for passing secret store instances across
//! the cdylib plugin boundary.

use std::ffi::c_void;

use super::types::FfiStr;

/// Result of `get_secret_fn`.
///
/// On success, `error_code == 0` and `value` is a valid C string with the secret.
/// On error, `error_code != 0` and `error_msg` describes the failure.
/// The caller must free `value` and `error_msg` by reconstructing CString via `from_raw`.
#[repr(C)]
pub struct FfiGetSecretResult {
    /// 0 = success, non-zero = error
    pub error_code: i32,
    /// Error message (null on success). Allocated via CString::into_raw.
    pub error_msg: *mut std::os::raw::c_char,
    /// Secret value (null on error). Allocated via CString::into_raw.
    pub value: *mut std::os::raw::c_char,
}

impl FfiGetSecretResult {
    /// Create a successful result with the given secret value.
    pub fn ok(value: String) -> Self {
        match std::ffi::CString::new(value) {
            Ok(c_value) => Self {
                error_code: 0,
                error_msg: std::ptr::null_mut(),
                value: c_value.into_raw(),
            },
            Err(_) => Self::err("secret value contained embedded null byte".to_string()),
        }
    }

    /// Create an error result.
    pub fn err(msg: String) -> Self {
        let c_msg = std::ffi::CString::new(msg).unwrap_or_else(|_| {
            // Safety: This literal contains no null bytes.
            std::ffi::CString::new("(error message contained null bytes)")
                .expect("static fallback message has no null bytes")
        });
        Self {
            error_code: 1,
            error_msg: c_msg.into_raw(),
            value: std::ptr::null_mut(),
        }
    }

    /// Convert to a Rust Result, consuming the FFI result.
    ///
    /// # Safety
    /// The caller must ensure this is called only once per instance.
    pub unsafe fn into_result(self) -> anyhow::Result<String> {
        if self.error_code == 0 {
            if self.value.is_null() {
                Ok(String::new())
            } else {
                let s = std::ffi::CStr::from_ptr(self.value)
                    .to_string_lossy()
                    .into_owned();
                // Free the CString allocation
                drop(std::ffi::CString::from_raw(self.value));
                Ok(s)
            }
        } else {
            let msg = if self.error_msg.is_null() {
                "Unknown error".to_string()
            } else {
                let s = std::ffi::CStr::from_ptr(self.error_msg)
                    .to_string_lossy()
                    .into_owned();
                drop(std::ffi::CString::from_raw(self.error_msg));
                s
            };
            // Free value if it was somehow allocated on error
            if !self.value.is_null() {
                drop(std::ffi::CString::from_raw(self.value));
            }
            Err(anyhow::anyhow!(msg))
        }
    }
}

/// FFI vtable for a SecretStoreProvider instance.
///
/// The plugin creates this wrapping a `Box<dyn SecretStoreProvider>`.
/// The host calls `get_secret_fn` to resolve named secrets.
#[repr(C)]
pub struct SecretStoreProviderVtable {
    /// Opaque pointer to the plugin-side provider state.
    pub state: *mut c_void,
    /// Fetch a secret by name. This is a blocking call — the plugin implementation
    /// spawns on the plugin runtime and blocks via channel.
    ///
    /// `name` is the secret name as a UTF-8 string.
    pub get_secret_fn: extern "C" fn(state: *const c_void, name: FfiStr) -> FfiGetSecretResult,
    /// Drop the state (releases the provider).
    pub drop_fn: extern "C" fn(state: *mut c_void),
}

// Safety: SecretStoreProviderVtable is designed to be passed across threads.
// The underlying state is a Box<dyn SecretStoreProvider> which is Send+Sync.
unsafe impl Send for SecretStoreProviderVtable {}
unsafe impl Sync for SecretStoreProviderVtable {}

#[cfg(test)]
mod tests {
    use super::FfiGetSecretResult;

    #[test]
    fn ok_returns_error_for_embedded_null_bytes() {
        let result = FfiGetSecretResult::ok("abc\0def".to_string());

        assert_eq!(result.error_code, 1);
        assert!(result.value.is_null());

        let err = unsafe { result.into_result() }.unwrap_err();
        assert_eq!(err.to_string(), "secret value contained embedded null byte");
    }

    #[test]
    fn err_uses_fallback_message_for_embedded_null_bytes() {
        let result = FfiGetSecretResult::err("bad\0message".to_string());

        assert_eq!(result.error_code, 1);
        assert!(result.value.is_null());

        let err = unsafe { result.into_result() }.unwrap_err();
        assert_eq!(err.to_string(), "(error message contained null bytes)");
    }
}
