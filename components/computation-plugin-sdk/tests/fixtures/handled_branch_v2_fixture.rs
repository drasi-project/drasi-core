// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[path = "handled_branch_fixture_common.rs"]
mod common;
use drasi_computation_plugin_sdk::{abi, transport, ExportedPlugin};
use std::sync::OnceLock;

fn exported() -> Result<&'static ExportedPlugin, &'static str> {
    static EXPORT: OnceLock<Result<ExportedPlugin, String>> = OnceLock::new();
    EXPORT
        .get_or_init(|| {
            common::definition()
                .and_then(ExportedPlugin::new)
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(String::as_str)
}

#[no_mangle]
pub extern "C" fn drasi_computation_plugin_metadata() -> *const abi::Metadata {
    exported().map_or(std::ptr::null(), ExportedPlugin::metadata)
}

#[no_mangle]
pub extern "C" fn drasi_computation_plugin_telemetry_v2() -> *const abi::telemetry::PluginTelemetryV2
{
    exported().map_or(std::ptr::null(), ExportedPlugin::telemetry_v2)
}

/// # Safety
/// `out` points to writable ABI plugin-handle storage.
#[no_mangle]
pub unsafe extern "C" fn drasi_computation_plugin_entry(
    out: *mut abi::PluginHandle,
) -> abi::Status {
    transport::status_boundary(|| {
        let export = exported().map_err(transport::Failure::failed)?;
        unsafe { export.entry(out) }
    })
}
