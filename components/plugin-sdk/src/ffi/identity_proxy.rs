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

//! Plugin-side proxy that implements `IdentityProvider` by calling through
//! an FFI vtable back into the host.

use std::ffi::c_void;

use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::identity::{CredentialContext, Credentials, IdentityProvider};

use super::identity::IdentityProviderVtable;

/// Plugin-side proxy wrapping an `IdentityProviderVtable` into the
/// `IdentityProvider` trait. Plugins use this to call the host's
/// identity provider through FFI.
///
/// The vtable is stored **by value** (not as a raw `Box` pointer), mirroring the
/// host-side `HostIdentityProviderProxy`. Storing by value means there is no separate
/// vtable-struct allocation to track or free, so `clone_box` cannot leak the struct and
/// `Drop` never has to (incorrectly) free host-owned memory across the cdylib boundary.
pub struct FfiIdentityProviderProxy {
    vtable: IdentityProviderVtable,
}

// Safety: The vtable points to host-allocated state (Arc<dyn IdentityProvider>)
// which is Send+Sync. The vtable itself is immutable after creation.
unsafe impl Send for FfiIdentityProviderProxy {}
unsafe impl Sync for FfiIdentityProviderProxy {}

impl FfiIdentityProviderProxy {
    /// Create a new proxy from a vtable pointer.
    ///
    /// The four vtable fields are copied **by value** out of `vtable`; the proxy does not
    /// retain the pointer. Ownership of the underlying state (`drop_fn` will be called on
    /// `Drop`) transfers to the proxy, but the caller's vtable-struct allocation remains
    /// owned by the caller and is never freed here.
    ///
    /// # Safety
    /// The vtable pointer must be non-null and valid for the duration of this call.
    pub unsafe fn new(vtable: *const IdentityProviderVtable) -> Self {
        let vtable = &*vtable;
        Self {
            vtable: IdentityProviderVtable {
                state: vtable.state,
                get_credentials_fn: vtable.get_credentials_fn,
                clone_fn: vtable.clone_fn,
                drop_fn: vtable.drop_fn,
            },
        }
    }
}

#[async_trait]
impl IdentityProvider for FfiIdentityProviderProxy {
    async fn get_credentials(&self, context: &CredentialContext) -> Result<Credentials> {
        // Serialize context as JSON for FFI transport
        let context_json =
            serde_json::to_string(&context.properties).unwrap_or_else(|_| "{}".to_string());
        let result = (self.vtable.get_credentials_fn)(
            self.vtable.state,
            context_json.as_ptr(),
            context_json.len(),
        );
        unsafe { result.into_result() }
    }

    fn clone_box(&self) -> Box<dyn IdentityProvider> {
        let cloned_state = (self.vtable.clone_fn)(self.vtable.state);

        // Build a new vtable value with freshly cloned state and the same function
        // pointers. No heap allocation for the vtable struct, so nothing to leak.
        Box::new(FfiIdentityProviderProxy {
            vtable: IdentityProviderVtable {
                state: cloned_state,
                get_credentials_fn: self.vtable.get_credentials_fn,
                clone_fn: self.vtable.clone_fn,
                drop_fn: self.vtable.drop_fn,
            },
        })
    }
}

impl Drop for FfiIdentityProviderProxy {
    fn drop(&mut self) {
        // Release the underlying host state (decrements the Arc refcount). There is no
        // separate vtable-struct allocation owned by this proxy, so nothing else to free.
        (self.vtable.drop_fn)(self.vtable.state);
    }
}
