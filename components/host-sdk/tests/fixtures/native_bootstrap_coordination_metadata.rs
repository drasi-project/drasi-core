// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A valid bootstrap library advertising an incomplete coordination extension.

mod native_bootstrap;

use drasi_computation_plugin_sdk::{self as sdk, abi};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock,
};

static ENTERED: AtomicBool = AtomicBool::new(false);

fn export() -> &'static sdk::ExportedPlugin {
    static EXPORT: OnceLock<sdk::ExportedPlugin> = OnceLock::new();
    EXPORT.get_or_init(|| {
        sdk::ExportedPlugin::new(
            sdk::PluginDefinition::new_with_bootstrap(
                "fixture/malformed-bootstrap-coordination",
                "1",
                Vec::new(),
                vec![Arc::new(native_bootstrap::Factory { progress: false })],
                vec![drasi_computation_contracts::GraphChangeCodec::schema()],
            )
            .expect("valid fixture definition"),
        )
        .expect("valid export")
    })
}

#[no_mangle]
pub extern "C" fn drasi_computation_plugin_metadata() -> *const abi::Metadata {
    std::panic::catch_unwind(|| export().metadata()).unwrap_or(std::ptr::null())
}

/// # Safety
/// `out` must be writable and unowned; release the returned native handle.
#[no_mangle]
pub unsafe extern "C" fn drasi_computation_plugin_entry(
    out: *mut abi::PluginHandle,
) -> abi::Status {
    ENTERED.store(true, Ordering::Release);
    sdk::transport::status_boundary(|| unsafe { export().entry(out) })
}

#[no_mangle]
pub extern "C" fn drasi_computation_plugin_bootstrap_v1() -> *const abi::bootstrap::PluginBootstrapV1
{
    export().bootstrap()
}

#[no_mangle]
pub extern "C" fn drasi_computation_plugin_bootstrap_coordination_v1(
) -> *const abi::bootstrap_coordination::PluginBootstrapCoordinationV1 {
    static TABLE: OnceLock<abi::bootstrap_coordination::PluginBootstrapCoordinationV1> =
        OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = unsafe { *export().bootstrap_coordination() };
        table.create_source = None;
        table
    })
}

#[no_mangle]
pub extern "C" fn drasi_fixture_entered() -> u8 {
    u8::from(ENTERED.load(Ordering::Acquire))
}
