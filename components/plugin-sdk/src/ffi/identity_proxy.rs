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
use drasi_lib::identity::{Credentials, IdentityProvider};

use super::identity::IdentityProviderVtable;

/// Plugin-side proxy wrapping an `IdentityProviderVtable` into the
/// `IdentityProvider` trait. Plugins use this to call the host's
/// identity provider through FFI.
pub struct FfiIdentityProviderProxy {
    vtable: *const IdentityProviderVtable,
}

// Safety: The vtable points to host-allocated state (Arc<dyn IdentityProvider>)
// which is Send+Sync. The vtable itself is immutable after creation.
unsafe impl Send for FfiIdentityProviderProxy {}
unsafe impl Sync for FfiIdentityProviderProxy {}

impl FfiIdentityProviderProxy {
    /// Create a new proxy from a vtable pointer.
    ///
    /// # Safety
    /// The vtable pointer must be valid and the vtable must outlive this proxy.
    /// The proxy takes ownership (will call drop_fn on Drop).
    pub unsafe fn new(vtable: *const IdentityProviderVtable) -> Self {
        Self { vtable }
    }
}

#[async_trait]
impl IdentityProvider for FfiIdentityProviderProxy {
    async fn get_credentials(&self) -> Result<Credentials> {
        let vtable = unsafe { &*self.vtable };
        // The get_credentials_fn is blocking (host spawns its own runtime)
        let result = (vtable.get_credentials_fn)(vtable.state);
        unsafe { result.into_result() }
    }

    fn clone_box(&self) -> Box<dyn IdentityProvider> {
        let vtable = unsafe { &*self.vtable };
        let cloned_state = (vtable.clone_fn)(vtable.state);

        // Build a new vtable with the cloned state but same function pointers
        let cloned_vtable = Box::new(IdentityProviderVtable {
            state: cloned_state,
            get_credentials_fn: vtable.get_credentials_fn,
            clone_fn: vtable.clone_fn,
            drop_fn: vtable.drop_fn,
        });

        Box::new(FfiIdentityProviderProxy {
            vtable: Box::into_raw(cloned_vtable),
        })
    }
}

impl Drop for FfiIdentityProviderProxy {
    fn drop(&mut self) {
        if !self.vtable.is_null() {
            let vtable = unsafe { &*self.vtable };
            (vtable.drop_fn)(vtable.state);
            // If we own the vtable (from clone_box), free it
            // Note: the original vtable from FfiRuntimeContext is host-owned
            // and should NOT be freed here. Only cloned vtables should be freed.
            // We handle this by always using Box::into_raw for cloned vtables.
        }
    }
}
