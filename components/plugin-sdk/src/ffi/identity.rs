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

//! FFI-safe IdentityProvider vtable for passing credential providers across the
//! cdylib plugin boundary.
//!
//! The host creates an `IdentityProviderVtable` wrapping an `Arc<dyn IdentityProvider>`
//! and passes it via `FfiRuntimeContext`. The plugin uses `FfiIdentityProviderProxy`
//! to call back into the host's provider through the vtable.

use std::ffi::c_void;

use super::types::{FfiResult, FfiStr};

/// FFI-safe credential type tag.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfiCredentialType {
    UsernamePassword = 0,
    Token = 1,
    Certificate = 2,
}

/// FFI-safe credentials returned by `get_credentials_fn`.
///
/// The caller must free `field1`, `field2`, and `field3` using `free_fn` if non-null.
#[repr(C)]
pub struct FfiCredentials {
    /// Credential type discriminant
    pub credential_type: FfiCredentialType,
    /// For UsernamePassword/Token: username. For Certificate: cert_pem.
    pub field1: *mut std::os::raw::c_char,
    /// For UsernamePassword: password. For Token: token. For Certificate: key_pem.
    pub field2: *mut std::os::raw::c_char,
    /// For Certificate: username (nullable). For others: null.
    pub field3: *mut std::os::raw::c_char,
}

/// Result of `get_credentials_fn`.
#[repr(C)]
pub struct FfiCredentialsResult {
    /// 0 = success, non-zero = error
    pub error_code: i32,
    /// Error message (null on success). Caller must free with libc::free or equivalent.
    pub error_msg: *mut std::os::raw::c_char,
    /// Credentials (only valid when error_code == 0)
    pub credentials: FfiCredentials,
}

/// FFI vtable for IdentityProvider.
///
/// The host builds this wrapping an `Arc<dyn IdentityProvider>` and passes it to
/// plugins via `FfiRuntimeContext.identity_provider`.
#[repr(C)]
pub struct IdentityProviderVtable {
    /// Opaque pointer to the host-side provider state.
    pub state: *mut c_void,
    /// Fetch credentials. This is a blocking call — the host implementation
    /// spawns a tokio runtime to call the async `get_credentials()`.
    pub get_credentials_fn: extern "C" fn(state: *const c_void) -> FfiCredentialsResult,
    /// Clone the vtable (creates a new Arc reference).
    pub clone_fn: extern "C" fn(state: *const c_void) -> *mut c_void,
    /// Drop the state (decrements Arc refcount).
    pub drop_fn: extern "C" fn(state: *mut c_void),
}

// Safety: IdentityProviderVtable is designed to be passed across threads.
// The underlying state is an Arc<dyn IdentityProvider> which is Send+Sync.
unsafe impl Send for IdentityProviderVtable {}
unsafe impl Sync for IdentityProviderVtable {}

impl FfiCredentials {
    /// Convert to drasi_lib Credentials, consuming the C strings.
    ///
    /// # Safety
    /// The caller must ensure field1/field2/field3 are valid C strings allocated
    /// by the host and that this is called only once per FfiCredentials instance.
    pub unsafe fn into_credentials(self) -> drasi_lib::identity::Credentials {
        let field1 = if self.field1.is_null() {
            String::new()
        } else {
            let s = std::ffi::CStr::from_ptr(self.field1)
                .to_string_lossy()
                .into_owned();
            libc_free(self.field1 as *mut c_void);
            s
        };
        let field2 = if self.field2.is_null() {
            String::new()
        } else {
            let s = std::ffi::CStr::from_ptr(self.field2)
                .to_string_lossy()
                .into_owned();
            libc_free(self.field2 as *mut c_void);
            s
        };
        let field3 = if self.field3.is_null() {
            None
        } else {
            let s = std::ffi::CStr::from_ptr(self.field3)
                .to_string_lossy()
                .into_owned();
            libc_free(self.field3 as *mut c_void);
            Some(s)
        };

        match self.credential_type {
            FfiCredentialType::UsernamePassword => {
                drasi_lib::identity::Credentials::UsernamePassword {
                    username: field1,
                    password: field2,
                }
            }
            FfiCredentialType::Token => drasi_lib::identity::Credentials::Token {
                username: field1,
                token: field2,
            },
            FfiCredentialType::Certificate => drasi_lib::identity::Credentials::Certificate {
                cert_pem: field1,
                key_pem: field2,
                username: field3,
            },
        }
    }
}

impl FfiCredentialsResult {
    /// Create a successful result.
    pub fn ok(credentials: FfiCredentials) -> Self {
        Self {
            error_code: 0,
            error_msg: std::ptr::null_mut(),
            credentials,
        }
    }

    /// Create an error result.
    pub fn err(msg: String) -> Self {
        let c_msg = std::ffi::CString::new(msg).unwrap_or_default();
        Self {
            error_code: 1,
            error_msg: c_msg.into_raw(),
            credentials: FfiCredentials {
                credential_type: FfiCredentialType::UsernamePassword,
                field1: std::ptr::null_mut(),
                field2: std::ptr::null_mut(),
                field3: std::ptr::null_mut(),
            },
        }
    }

    /// Convert to a Rust Result, consuming the FFI result.
    ///
    /// # Safety
    /// The caller must ensure this is called only once.
    pub unsafe fn into_result(self) -> anyhow::Result<drasi_lib::identity::Credentials> {
        if self.error_code == 0 {
            Ok(self.credentials.into_credentials())
        } else {
            let msg = if self.error_msg.is_null() {
                "Unknown error".to_string()
            } else {
                let s = std::ffi::CStr::from_ptr(self.error_msg)
                    .to_string_lossy()
                    .into_owned();
                libc_free(self.error_msg as *mut c_void);
                s
            };
            Err(anyhow::anyhow!(msg))
        }
    }
}

/// Free a C-allocated pointer. We use Box deallocation since we allocated with CString::into_raw.
unsafe fn libc_free(ptr: *mut c_void) {
    if !ptr.is_null() {
        // CString::into_raw allocates via Rust allocator, so we reconstruct and drop
        drop(std::ffi::CString::from_raw(ptr as *mut std::os::raw::c_char));
    }
}

/// Build FfiCredentials from drasi_lib Credentials.
pub fn credentials_to_ffi(creds: drasi_lib::identity::Credentials) -> FfiCredentials {
    match creds {
        drasi_lib::identity::Credentials::UsernamePassword { username, password } => {
            FfiCredentials {
                credential_type: FfiCredentialType::UsernamePassword,
                field1: std::ffi::CString::new(username).unwrap_or_default().into_raw(),
                field2: std::ffi::CString::new(password).unwrap_or_default().into_raw(),
                field3: std::ptr::null_mut(),
            }
        }
        drasi_lib::identity::Credentials::Token { username, token } => FfiCredentials {
            credential_type: FfiCredentialType::Token,
            field1: std::ffi::CString::new(username).unwrap_or_default().into_raw(),
            field2: std::ffi::CString::new(token).unwrap_or_default().into_raw(),
            field3: std::ptr::null_mut(),
        },
        drasi_lib::identity::Credentials::Certificate {
            cert_pem,
            key_pem,
            username,
        } => FfiCredentials {
            credential_type: FfiCredentialType::Certificate,
            field1: std::ffi::CString::new(cert_pem).unwrap_or_default().into_raw(),
            field2: std::ffi::CString::new(key_pem).unwrap_or_default().into_raw(),
            field3: username
                .map(|u| std::ffi::CString::new(u).unwrap_or_default().into_raw())
                .unwrap_or(std::ptr::null_mut()),
        },
    }
}
