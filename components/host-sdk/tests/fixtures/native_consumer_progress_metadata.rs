// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Intentionally malformed optional table; metadata scans must not enter it.

mod native_consumer_progress;

use drasi_computation_plugin_sdk::{self as sdk, abi};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    OnceLock,
};

static ENTERED: AtomicBool = AtomicBool::new(false);

fn export() -> &'static sdk::ExportedPlugin {
    static EXPORT: OnceLock<sdk::ExportedPlugin> = OnceLock::new();
    EXPORT.get_or_init(|| {
        sdk::ExportedPlugin::new(
            sdk::PluginDefinition::new(
                "fixture/malformed-consumer-progress",
                "1",
                native_consumer_progress::factories(),
                vec![drasi_computation_contracts::GraphChangeCodec::schema()],
            )
            .expect("valid fixture definition"),
        )
        .expect("valid fixture export")
    })
}

#[no_mangle]
pub extern "C" fn drasi_computation_plugin_metadata() -> *const abi::Metadata {
    std::panic::catch_unwind(|| export().metadata()).unwrap_or(std::ptr::null())
}

/// # Safety
/// `out` must be writable and unowned; release the returned handle through its
/// producer callback as required by native ABI 1.0.
#[no_mangle]
pub unsafe extern "C" fn drasi_computation_plugin_entry(
    out: *mut abi::PluginHandle,
) -> abi::Status {
    ENTERED.store(true, Ordering::Release);
    sdk::transport::status_boundary(|| unsafe { export().entry(out) })
}

#[no_mangle]
pub extern "C" fn drasi_computation_plugin_consumer_progress_v1(
) -> *const abi::consumer_progress::PluginConsumerProgressV1 {
    static TABLE: OnceLock<abi::consumer_progress::PluginConsumerProgressV1> = OnceLock::new();
    std::panic::catch_unwind(|| {
        TABLE.get_or_init(|| {
            let mut table = unsafe { *export().consumer_progress() };
            table.position_policy = None;
            table
        }) as *const _
    })
    .unwrap_or(std::ptr::null())
}

#[no_mangle]
pub extern "C" fn drasi_fixture_entered() -> u8 {
    u8::from(ENTERED.load(Ordering::Acquire))
}
