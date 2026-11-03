// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod execution_context_common;
use drasi_computation_plugin_sdk::{abi, transport, ExportedPlugin};
use std::sync::OnceLock;

fn exported() -> &'static ExportedPlugin {
    static EXPORTED: OnceLock<ExportedPlugin> = OnceLock::new();
    EXPORTED.get_or_init(|| {
        ExportedPlugin::new(execution_context_common::definition().expect("fixture definition"))
            .expect("fixture exports")
    })
}

#[no_mangle]
pub extern "C" fn drasi_computation_plugin_metadata() -> *const abi::Metadata {
    std::panic::catch_unwind(|| exported().metadata()).unwrap_or(std::ptr::null())
}

#[no_mangle]
/// # Safety
/// `out` must be a writable component-plugin handle output.
pub unsafe extern "C" fn drasi_computation_plugin_entry(
    out: *mut abi::PluginHandle,
) -> abi::Status {
    transport::status_boundary(|| unsafe { exported().entry(out) })
}
