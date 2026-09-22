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

use std::path::{Path, PathBuf};
use std::process::Command;

use drasi_host_sdk::callbacks;
use drasi_host_sdk::loader::load_plugin_from_path;
use libloading::{Library, Symbol};

fn fixture_library_path(directory: &Path) -> PathBuf {
    if cfg!(target_os = "windows") {
        directory.join("source_vtable_0_14.dll")
    } else if cfg!(target_os = "macos") {
        directory.join("libsource_vtable_0_14.dylib")
    } else {
        directory.join("libsource_vtable_0_14.so")
    }
}

fn compile_fixture(configuration: Option<&str>) -> (tempfile::TempDir, PathBuf) {
    let fixture_source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/source_vtable_0_14.rs");
    let output_dir = tempfile::tempdir().expect("fixture output directory");
    let fixture_library = fixture_library_path(output_dir.path());
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let mut command = Command::new(rustc);
    command
        .arg("--crate-type=cdylib")
        .arg("--edition=2021")
        .arg(&fixture_source)
        .arg("-o")
        .arg(&fixture_library);
    if let Some(configuration) = configuration {
        command.arg("--cfg").arg(configuration);
    }
    let status = command.status().expect("compile old ABI fixture");
    assert!(status.success(), "old ABI fixture should compile");
    (output_dir, fixture_library)
}

#[test]
fn loader_rejects_smaller_source_vtable_before_plugin_init() {
    let (_output_dir, fixture_library) = compile_fixture(None);
    let fixture_handle =
        unsafe { Library::new(&fixture_library) }.expect("load old ABI fixture tripwire");
    let init_called: Symbol<unsafe extern "C" fn() -> bool> = unsafe {
        fixture_handle
            .get(b"old_abi_fixture_init_called")
            .expect("resolve fixture tripwire")
    };
    let source_vtable_size: Symbol<unsafe extern "C" fn() -> usize> = unsafe {
        fixture_handle
            .get(b"old_abi_fixture_source_vtable_size")
            .expect("resolve old vtable size")
    };
    assert!(
        unsafe { source_vtable_size() }
            < std::mem::size_of::<drasi_plugin_sdk::ffi::SourceVtable>(),
        "fixture must represent the physically smaller pre-0.15 source vtable"
    );

    let error = match load_plugin_from_path(
        &fixture_library,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    ) {
        Ok(_) => panic!("0.14 plugin must be rejected before its smaller vtable is read"),
        Err(error) => error,
    };

    assert!(
        error.to_string().contains("SDK version mismatch"),
        "expected explicit ABI rejection, got: {error:#}"
    );
    assert!(
        !unsafe { init_called() },
        "loader must reject incompatible metadata before drasi_plugin_init"
    );
}

#[test]
fn loader_rejects_missing_metadata_before_plugin_init() {
    let (_output_dir, fixture_library) = compile_fixture(Some("omit_metadata"));
    let fixture_handle =
        unsafe { Library::new(&fixture_library) }.expect("load metadata-free fixture tripwire");
    let init_called: Symbol<unsafe extern "C" fn() -> bool> = unsafe {
        fixture_handle
            .get(b"old_abi_fixture_init_called")
            .expect("resolve fixture tripwire")
    };

    let error = match load_plugin_from_path(
        &fixture_library,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    ) {
        Ok(_) => panic!("plugin without metadata must be rejected before initialization"),
        Err(error) => error,
    };

    assert!(
        error
            .to_string()
            .contains("does not export drasi_plugin_metadata"),
        "expected explicit missing-metadata rejection, got: {error:#}"
    );
    assert!(
        !unsafe { init_called() },
        "loader must reject missing metadata before drasi_plugin_init"
    );
}
