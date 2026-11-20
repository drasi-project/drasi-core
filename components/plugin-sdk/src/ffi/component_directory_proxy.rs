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

//! Plugin-side proxy for the host's component directory.

use super::types::FfiStr;
use super::vtables::ComponentDirectoryVtable;

/// Wraps a host-provided `ComponentDirectoryVtable` as a `ComponentDirectory`.
pub struct FfiComponentDirectoryProxy {
    pub(crate) vtable: *const ComponentDirectoryVtable,
}

unsafe impl Send for FfiComponentDirectoryProxy {}
unsafe impl Sync for FfiComponentDirectoryProxy {}

impl Drop for FfiComponentDirectoryProxy {
    fn drop(&mut self) {
        if !self.vtable.is_null() {
            let vtable = unsafe { &*self.vtable };
            (vtable.drop_fn)(vtable.state);
        }
    }
}

impl FfiComponentDirectoryProxy {
    fn lookup(
        &self,
        select: impl Fn(
            &ComponentDirectoryVtable,
        )
            -> extern "C" fn(*mut std::ffi::c_void, FfiStr) -> super::types::FfiOwnedStr,
        key: &str,
    ) -> Option<String> {
        let vtable = unsafe { &*self.vtable };
        let value = unsafe { select(vtable)(vtable.state, FfiStr::from_str(key)).into_string() };
        (!value.is_empty()).then_some(value)
    }
}

impl drasi_lib::ComponentDirectory for FfiComponentDirectoryProxy {
    fn name_of(&self, id: &drasi_lib::ComponentId) -> Option<drasi_lib::ComponentName> {
        self.lookup(|vtable| vtable.name_of_fn, id.as_str())
            .and_then(|name| drasi_lib::ComponentName::try_new(name).ok())
    }

    fn id_of(&self, name: &drasi_lib::ComponentName) -> Option<drasi_lib::ComponentId> {
        self.lookup(|vtable| vtable.id_of_fn, name.as_str())
            .and_then(|id| drasi_lib::ComponentId::try_new(id).ok())
    }
}
