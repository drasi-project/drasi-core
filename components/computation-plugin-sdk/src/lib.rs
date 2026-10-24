// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native ComputationGraph SDK. See the crate README for the ownership, scheduling
//! and capability contracts. No legacy Source/Reaction interfaces are involved.

pub use drasi_computation_contracts::SinkBatching;
pub use drasi_computation_plugin_abi as abi;
pub mod activation_credentials;
pub mod admission;
pub mod bootstrap;
pub mod bootstrap_coordination;
mod component;
pub mod configuration;
pub mod consumer;
pub mod consumer_progress;
mod control;
mod export;
pub mod input_position;
pub mod metadata;
pub mod progress;
pub mod query_snapshots;
mod transaction;
pub mod transport;
pub mod wire;

pub use activation_credentials::{ActivationCredentialContext, ActivationCredentials};
pub use admission::{AdmissionError, AdmissionErrorKind, NativeAdmission};
pub use bootstrap::{BootstrapFactory, BootstrapFactoryMetadata, NativeBootstrapProvider};
pub use bootstrap_coordination::{
    LinkedBootstrapProvider, NativeSourceBootstrap, NativeSourceBootstrapCoordinator,
    SourceBootstrapLiveProgress, SourceBootstrapMetadata, SourceBootstrapProgress,
};
pub use component::{NativeComponent, NativeService, NativeSink, NativeSource, NativeTransformer};
pub use configuration::{ConfigDefinition, ConfigDetails, ConfigOption};
pub use consumer::{ConsumerMode, NativeConsumer, NativeTransactionalConsumer};
pub use consumer_progress::NativeConsumerProgress;
pub use control::{
    ControlDirection, ControlMessage, ControlNotification, ControlSender, ControlTarget,
    NativeControlHandler, SendControl,
};
pub use export::{Component, CreatedComponent, ExportedPlugin, Factory, PluginDefinition};
pub use metadata::{
    Capabilities, ConfigField, ConfigSchema, ConfigType, FactoryMetadata, PluginMetadata,
};
pub use progress::NativeSourceProgress;
pub use query_snapshots::{NativeQuerySnapshot, NativeQuerySnapshots, QuerySnapshotMetadata};
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
        pub extern "C" fn drasi_computation_plugin_configuration_v1(
        ) -> *const $crate::abi::configuration::ConfigurationMetadataV1 {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match __drasi_computation_export() {
                    Ok(export) => export.configuration(),
                    Err(_) => std::ptr::null(),
                }
            }))
            .unwrap_or(std::ptr::null())
        }
        #[no_mangle]
        pub extern "C" fn drasi_computation_plugin_services_v1(
        ) -> *const $crate::abi::services::PluginServicesV1 {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match __drasi_computation_export() {
                    Ok(export) => export.services(),
                    Err(_) => std::ptr::null(),
                }
            }))
            .unwrap_or(std::ptr::null())
        }
        #[no_mangle]
        pub extern "C" fn drasi_computation_plugin_batching_v1(
        ) -> *const $crate::abi::batching::PluginBatchingV1 {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match __drasi_computation_export() {
                    Ok(export) => export.batching(),
                    Err(_) => std::ptr::null(),
                }
            }))
            .unwrap_or(std::ptr::null())
        }
        #[no_mangle]
        pub extern "C" fn drasi_computation_plugin_query_snapshots_v1(
        ) -> *const $crate::abi::query_snapshots::PluginQuerySnapshotsV1 {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match __drasi_computation_export() {
                    Ok(export) => export.query_snapshots(),
                    Err(_) => std::ptr::null(),
                }
            }))
            .unwrap_or(std::ptr::null())
        }
        #[no_mangle]
        pub extern "C" fn drasi_computation_plugin_recovery_v1(
        ) -> *const $crate::abi::recovery::PluginRecoveryV1 {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match __drasi_computation_export() {
                    Ok(export) => export.recovery(),
                    Err(_) => std::ptr::null(),
                }
            }))
            .unwrap_or(std::ptr::null())
        }
        #[no_mangle]
        pub extern "C" fn drasi_computation_plugin_consumer_progress_v1(
        ) -> *const $crate::abi::consumer_progress::PluginConsumerProgressV1 {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match __drasi_computation_export() {
                    Ok(export) => export.consumer_progress(),
                    Err(_) => std::ptr::null(),
                }
            }))
            .unwrap_or(std::ptr::null())
        }
        #[no_mangle]
        pub extern "C" fn drasi_computation_plugin_activation_credentials_v1(
        ) -> *const $crate::abi::activation_credentials::PluginActivationCredentialsV1 {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match __drasi_computation_export() {
                    Ok(export) => export.activation_credentials(),
                    Err(_) => std::ptr::null(),
                }
            }))
            .unwrap_or(std::ptr::null())
        }
        #[no_mangle]
        pub extern "C" fn drasi_computation_plugin_source_schema_v1(
        ) -> *const $crate::abi::source_schema::PluginSourceSchemaV1 {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match __drasi_computation_export() {
                    Ok(export) => export.source_schema(),
                    Err(_) => std::ptr::null(),
                }
            }))
            .unwrap_or(std::ptr::null())
        }
        #[no_mangle]
        pub extern "C" fn drasi_computation_plugin_bootstrap_coordination_v1(
        ) -> *const $crate::abi::bootstrap_coordination::PluginBootstrapCoordinationV1 {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match __drasi_computation_export() {
                    Ok(export) => export.bootstrap_coordination(),
                    Err(_) => std::ptr::null(),
                }
            }))
            .unwrap_or(std::ptr::null())
        }
        #[no_mangle]
        pub extern "C" fn drasi_computation_plugin_bootstrap_v1(
        ) -> *const $crate::abi::bootstrap::PluginBootstrapV1 {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match __drasi_computation_export() {
                    Ok(export) => export.bootstrap(),
                    Err(_) => std::ptr::null(),
                }
            }))
            .unwrap_or(std::ptr::null())
        }
        #[no_mangle]
        pub extern "C" fn drasi_computation_plugin_consumer_v1(
        ) -> *const $crate::abi::consumer::PluginConsumerV1 {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match __drasi_computation_export() {
                    Ok(export) => export.consumer(),
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
