// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native ComputationGraph SDK. See the crate README for the ownership, scheduling
//! and capability contracts. No legacy Source/Reaction interfaces are involved.

pub use drasi_computation_plugin_abi as abi;
mod control;
mod export;
pub mod metadata;
mod transaction;
pub mod transport;
pub mod wire;

pub use control::{
    ControlDirection, ControlMessage, ControlNotification, ControlSender, ControlTarget,
    NativeControlHandler, SendControl,
};
pub use export::{Component, CreatedComponent, ExportedPlugin, Factory, PluginDefinition};
pub use metadata::{
    Capabilities, ConfigField, ConfigSchema, ConfigType, FactoryMetadata, PluginMetadata,
};
pub use transaction::{NativeTransactionContext, TransactionalComponent};
pub use wire::{CreateRequest, Scope};

/// Export only under a plugin crate's `dynamic-plugin` feature. The expression is
/// evaluated once and must return `anyhow::Result<PluginDefinition>`. It must be
/// configuration-only: no I/O, processing tasks or graph scheduling.
#[macro_export]
macro_rules! export_computation_plugin {
    ($definition:expr) => {
        fn __drasi_computation_export(
        ) -> &'static std::result::Result<$crate::ExportedPlugin, String> {
            static EXPORT: std::sync::OnceLock<
                std::result::Result<$crate::ExportedPlugin, String>,
            > = std::sync::OnceLock::new();
            EXPORT.get_or_init(|| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    ($definition)
                        .and_then($crate::ExportedPlugin::new)
                        .map_err(|error| format!("{error:#}"))
                }))
                .unwrap_or_else(|_| Err("native plugin definition panicked".into()))
            })
        }
        #[no_mangle]
        pub extern "C" fn drasi_computation_plugin_metadata() -> *const $crate::abi::Metadata {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match __drasi_computation_export() {
                    Ok(export) => export.metadata(),
                    Err(_) => std::ptr::null(),
                }
            }))
            .unwrap_or(std::ptr::null())
        }
        #[no_mangle]
        pub unsafe extern "C" fn drasi_computation_plugin_entry(
            out: *mut $crate::abi::PluginHandle,
        ) -> $crate::abi::Status {
            $crate::transport::status_boundary(|| match __drasi_computation_export() {
                Ok(export) => unsafe { export.entry(out) },
                Err(error) => Err($crate::transport::Failure::failed(error.clone())),
            })
        }
    };
}
