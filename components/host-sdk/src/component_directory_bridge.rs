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

//! Bridge from a host-side `ComponentDirectory` to the FFI `ComponentDirectoryVtable`.

use std::ffi::c_void;
use std::sync::Arc;

use drasi_lib::ComponentDirectory;
use drasi_plugin_sdk::ffi::{ComponentDirectoryVtable, FfiOwnedStr, FfiStr};

/// Builds a `ComponentDirectoryVtable` that dispatches to a host directory.
pub struct ComponentDirectoryVtableBuilder;

impl ComponentDirectoryVtableBuilder {
    /// The vtable holds an `Arc` clone until the plugin calls `drop_fn`.
    pub fn build(directory: Arc<dyn ComponentDirectory>) -> ComponentDirectoryVtable {
        ComponentDirectoryVtable {
            state: Box::into_raw(Box::new(directory)) as *mut c_void,
            name_of_fn: cd_name_of,
            id_of_fn: cd_id_of,
            drop_fn: cd_drop,
        }
    }
}

fn directory(state: *mut c_void) -> &'static dyn ComponentDirectory {
    let directory = unsafe { &*(state as *const Arc<dyn ComponentDirectory>) };
    directory.as_ref()
}

extern "C" fn cd_name_of(state: *mut c_void, id: FfiStr) -> FfiOwnedStr {
    let id = unsafe { id.to_string() };
    let name = drasi_lib::ComponentId::try_new(id)
        .ok()
        .and_then(|id| directory(state).name_of(&id))
        .map(|name| name.to_string())
        .unwrap_or_default();
    FfiOwnedStr::from_string(name)
}

extern "C" fn cd_id_of(state: *mut c_void, name: FfiStr) -> FfiOwnedStr {
    let name = unsafe { name.to_string() };
    let id = drasi_lib::ComponentName::try_new(name)
        .ok()
        .and_then(|name| directory(state).id_of(&name))
        .map(|id| id.to_string())
        .unwrap_or_default();
    FfiOwnedStr::from_string(id)
}

extern "C" fn cd_drop(state: *mut c_void) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state as *mut Arc<dyn ComponentDirectory>) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed {
        id: drasi_lib::ComponentId,
        name: drasi_lib::ComponentName,
    }

    impl ComponentDirectory for Fixed {
        fn name_of(&self, id: &drasi_lib::ComponentId) -> Option<drasi_lib::ComponentName> {
            (id == &self.id).then(|| self.name.clone())
        }
        fn id_of(&self, name: &drasi_lib::ComponentName) -> Option<drasi_lib::ComponentId> {
            (name == &self.name).then(|| self.id.clone())
        }
    }

    #[test]
    fn vtable_round_trips_lookups() {
        let id = drasi_lib::ComponentId::generate();
        let name = drasi_lib::ComponentName::try_new("values").unwrap();
        let vtable = ComponentDirectoryVtableBuilder::build(Arc::new(Fixed {
            id: id.clone(),
            name: name.clone(),
        }));
        let found = unsafe {
            (vtable.name_of_fn)(vtable.state, FfiStr::from_str(id.as_str())).into_string()
        };
        assert_eq!(found, "values");
        let found =
            unsafe { (vtable.id_of_fn)(vtable.state, FfiStr::from_str("values")).into_string() };
        assert_eq!(found, id.as_str());
        let missing =
            unsafe { (vtable.id_of_fn)(vtable.state, FfiStr::from_str("other")).into_string() };
        assert!(missing.is_empty());
        (vtable.drop_fn)(vtable.state);
    }
}
