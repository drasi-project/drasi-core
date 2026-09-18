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

#![allow(dead_code)]

use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

#[repr(C)]
struct FfiStr {
    ptr: *const c_char,
    len: usize,
}

unsafe impl Sync for FfiStr {}

const fn ffi_str(value: &'static [u8]) -> FfiStr {
    FfiStr {
        ptr: value.as_ptr().cast(),
        len: value.len(),
    }
}

#[repr(C)]
struct PluginMetadata {
    sdk_version: FfiStr,
    core_version: FfiStr,
    lib_version: FfiStr,
    plugin_version: FfiStr,
    target_triple: FfiStr,
    git_commit: FfiStr,
    build_timestamp: FfiStr,
}

unsafe impl Sync for PluginMetadata {}

static METADATA: PluginMetadata = PluginMetadata {
    sdk_version: ffi_str(b"0.14.0"),
    core_version: ffi_str(b"0.0.0"),
    lib_version: ffi_str(b"0.0.0"),
    plugin_version: ffi_str(b"0.0.0"),
    target_triple: ffi_str(b"old-abi-fixture"),
    git_commit: ffi_str(b"fixture"),
    build_timestamp: ffi_str(b"1970-01-01T00:00:00Z"),
};

static INIT_CALLED: AtomicBool = AtomicBool::new(false);

// This is the physical 0.14 shape: drop_fn immediately follows
// remove_position_handle_fn, with no on_subscriptions_complete_fn slot.
#[allow(dead_code)]
#[repr(C)]
struct SourceVtableV014 {
    state: *mut c_void,
    executor: usize,
    id_fn: usize,
    type_name_fn: usize,
    auto_start_fn: usize,
    dispatch_mode_fn: usize,
    properties_fn: usize,
    describe_schema_fn: usize,
    start_fn: usize,
    stop_fn: usize,
    status_fn: usize,
    deprovision_fn: usize,
    initialize_fn: usize,
    subscribe_fn: usize,
    set_bootstrap_provider_fn: usize,
    supports_replay_fn: usize,
    remove_position_handle_fn: usize,
    drop_fn: usize,
}

#[cfg(not(omit_metadata))]
#[unsafe(no_mangle)]
extern "C" fn drasi_plugin_metadata() -> *const PluginMetadata {
    &METADATA
}

#[unsafe(no_mangle)]
extern "C" fn drasi_plugin_init() -> *mut c_void {
    INIT_CALLED.store(true, Ordering::SeqCst);
    std::ptr::null_mut()
}

#[unsafe(no_mangle)]
extern "C" fn old_abi_fixture_init_called() -> bool {
    INIT_CALLED.load(Ordering::SeqCst)
}

#[unsafe(no_mangle)]
extern "C" fn old_abi_fixture_source_vtable_size() -> usize {
    std::mem::size_of::<SourceVtableV014>()
}
