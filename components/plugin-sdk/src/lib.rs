#![allow(unexpected_cfgs)]
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

//! # Drasi Plugin SDK
//!
//! The Drasi Plugin SDK provides the traits, types, and utilities needed to build
//! plugins for the Drasi Server. Plugins can be compiled directly into the server
//! binary (static linking) or built as shared libraries for dynamic loading.
//!
//! ## Quick Start
//!
//! ```rust,ignore
//! use drasi_plugin_sdk::prelude::*;
//!
//! // 1. Define your configuration DTO with OpenAPI schema support
//! #[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
//! #[serde(rename_all = "camelCase")]
//! pub struct MySourceConfigDto {
//!     /// The hostname to connect to
//!     #[schema(value_type = ConfigValueString)]
//!     pub host: ConfigValue<String>,
//!
//!     /// The port number
//!     #[schema(value_type = ConfigValueU16)]
//!     pub port: ConfigValue<u16>,
//!
//!     /// Optional timeout in milliseconds
//!     #[serde(skip_serializing_if = "Option::is_none")]
//!     #[schema(value_type = Option<ConfigValueU32>)]
//!     pub timeout_ms: Option<ConfigValue<u32>>,
//! }
//!
//! // 2. Implement the appropriate descriptor trait
//! pub struct MySourceDescriptor;
//!
//! #[async_trait]
//! impl SourcePluginDescriptor for MySourceDescriptor {
//!     fn kind(&self) -> &str { "my-source" }
//!     fn config_version(&self) -> &str { "1.0.0" }
//!
//!     fn config_schema_json(&self) -> String {
//!         let schema = <MySourceConfigDto as utoipa::ToSchema>::schema();
//!         serde_json::to_string(&schema).unwrap()
//!     }
//!
//!     async fn create_source(
//!         &self,
//!         id: &str,
//!         config_json: &serde_json::Value,
//!         auto_start: bool,
//!     ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
//!         let dto: MySourceConfigDto = serde_json::from_value(config_json.clone())?;
//!         let mapper = DtoMapper::new();
//!         let host = mapper.resolve_string(&dto.host)?;
//!         let port = mapper.resolve_typed(&dto.port)?;
//!         // Build and return your source implementation...
//!         todo!()
//!     }
//! }
//!
//! // 3. Create a plugin registration
//! pub fn register() -> PluginRegistration {
//!     PluginRegistration::new()
//!         .with_source(Box::new(MySourceDescriptor))
//! }
//! ```
//!
//! ## Static vs. Dynamic Plugins
//!
//! Plugins can be integrated with Drasi Server in two ways:
//!
//! ### Static Linking
//!
//! Compile the plugin directly into the server binary. Create a
//! [`PluginRegistration`](registration::PluginRegistration) and pass its descriptors
//! to the server's plugin registry at startup. This is the simplest approach and
//! is shown in the Quick Start above.
//!
//! ### Dynamic Loading
//!
//! Build the plugin as a shared library (`cdylib`) that the server loads at runtime
//! from a plugins directory. This allows deploying new plugins without recompiling
//! the server. See [Creating a Dynamic Plugin](#creating-a-dynamic-plugin) below
//! for the full workflow.
//!
//! ## Creating a Dynamic Plugin
//!
//! Dynamic plugins are compiled as shared libraries (`.so` on Linux, `.dylib` on
//! macOS, `.dll` on Windows) and placed in the server's plugins directory. The server
//! discovers and loads them automatically at startup.
//!
//! ### Step 1: Set up the crate
//!
//! In your plugin's `Cargo.toml`, set the crate type to `cdylib`:
//!
//! ```toml
//! [lib]
//! crate-type = ["cdylib"]
//!
//! [dependencies]
//! drasi-plugin-sdk = "..."  # Must match the server's version exactly
//! drasi-lib = "..."
//! ```
//!
//! ### Step 2: Implement descriptor(s)
//!
//! Implement [`SourcePluginDescriptor`](descriptor::SourcePluginDescriptor),
//! [`ReactionPluginDescriptor`](descriptor::ReactionPluginDescriptor), and/or
//! [`BootstrapPluginDescriptor`](descriptor::BootstrapPluginDescriptor) for your
//! plugin. See the [`descriptor`] module docs for the full trait requirements.
//!
//! ### Step 3: Export the entry point
//!
//! Every dynamic plugin shared library **must** export a C function named
//! `drasi_plugin_init` that returns a heap-allocated
//! [`PluginRegistration`](registration::PluginRegistration) via raw pointer:
//!
//! ```rust,ignore
//! use drasi_plugin_sdk::prelude::*;
//!
//! #[no_mangle]
//! pub extern "C" fn drasi_plugin_init() -> *mut PluginRegistration {
//!     let registration = PluginRegistration::new()
//!         .with_source(Box::new(MySourceDescriptor))
//!         .with_reaction(Box::new(MyReactionDescriptor));
//!     Box::into_raw(Box::new(registration))
//! }
//! ```
//!
//! **Important details:**
//!
//! - The function must be `#[no_mangle]` and `extern "C"` so the server can find it
//!   via the C ABI.
//! - The `PluginRegistration` must be heap-allocated with `Box::new` and returned as
//!   a raw pointer via [`Box::into_raw`]. The server takes ownership by calling
//!   `Box::from_raw`.
//! - The [`PluginRegistration::new()`](registration::PluginRegistration::new) constructor
//!   automatically embeds the [`SDK_VERSION`](registration::SDK_VERSION) constant.
//!   The server checks this at load time and **rejects plugins built with a different
//!   SDK version**.
//!
//! ### Step 4: Build and deploy
//!
//! ```bash
//! cargo build --release
//! # Copy the shared library to the server's plugins directory
//! cp target/release/libmy_plugin.so /path/to/plugins/
//! ```
//!
//! ### Compatibility Requirements
//!
//! Both the plugin and the server **must** be compiled with:
//!
//! - The **same Rust toolchain** version (the Rust ABI is not stable across versions).
//! - The **same `drasi-plugin-sdk` version**. The server compares
//!   [`SDK_VERSION`](registration::SDK_VERSION) at load time and rejects mismatches.
//!
//! Failing to meet these requirements will result in the plugin being rejected at
//! load time or, in the worst case, undefined behavior from ABI incompatibility.
//!
//! ## Modules
//!
//! - [`config_value`] — The [`ConfigValue<T>`](config_value::ConfigValue) enum for
//!   configuration fields that support static values, environment variables, and secrets.
//! - [`resolver`] — Value resolvers that convert config references to actual values.
//! - [`mapper`] — The [`DtoMapper`](mapper::DtoMapper) service and [`ConfigMapper`](mapper::ConfigMapper)
//!   trait for DTO-to-domain conversions.
//! - [`descriptor`] — Plugin descriptor traits
//!   ([`SourcePluginDescriptor`](descriptor::SourcePluginDescriptor),
//!   [`ReactionPluginDescriptor`](descriptor::ReactionPluginDescriptor),
//!   [`BootstrapPluginDescriptor`](descriptor::BootstrapPluginDescriptor)).
//! - [`registration`] — The [`PluginRegistration`](registration::PluginRegistration) struct
//!   returned by plugin entry points.
//! - [`prelude`] — Convenience re-exports for plugin authors.
//!
//! ## Configuration Values
//!
//! Plugin DTOs use [`ConfigValue<T>`](config_value::ConfigValue) for fields that may
//! be provided as static values, environment variable references, or secret references.
//! See the [`config_value`] module for the full documentation and supported formats.
//!
//! ## OpenAPI Schema Generation
//!
//! Each plugin provides its configuration schema as a JSON-serialized utoipa `Schema`.
//! The server deserializes these schemas and assembles them into the OpenAPI specification.
//! This approach preserves strongly-typed OpenAPI documentation while keeping schema
//! ownership with the plugins.
//!
//! ## DTO Versioning
//!
//! Each plugin independently versions its configuration DTO using semver. The server
//! tracks config versions and can reject incompatible plugins. See the [`descriptor`]
//! module docs for versioning rules.

pub mod config_value;
pub mod descriptor;
pub mod ffi;
pub mod mapper;
pub mod prelude;
pub mod registration;
pub mod resolver;

// Top-level re-exports for convenience
pub use config_value::ConfigValue;
pub use descriptor::{BootstrapPluginDescriptor, ReactionPluginDescriptor, SourcePluginDescriptor};
pub use mapper::{ConfigMapper, DtoMapper, MappingError};
pub use registration::{PluginRegistration, SDK_VERSION};
pub use resolver::{register_secret_resolver, ResolverError};

/// Re-export tokio so the `export_plugin!` macro can reference it
/// without requiring plugins to declare a direct tokio dependency.
#[doc(hidden)]
pub use tokio as __tokio;

/// Export dynamic plugin entry points with FFI vtables.
///
/// Generates:
/// - `drasi_plugin_metadata()` → version info for validation
/// - `drasi_plugin_init()` → `FfiPluginRegistration` with vtable factories
/// - Plugin-local tokio runtime, FfiLogger, lifecycle callbacks
///
/// # Usage
///
/// ```rust,ignore
/// drasi_plugin_sdk::export_plugin!(
///     plugin_id = "postgres",
///     core_version = "0.1.0",
///     lib_version = "0.3.8",
///     plugin_version = "1.0.0",
///     source_descriptors = [PostgresSourceDescriptor],
///     reaction_descriptors = [],
///     bootstrap_descriptors = [PostgresBootstrapDescriptor],
/// );
/// ```
#[macro_export]
macro_rules! export_plugin {
    // ── Declarative form: descriptors listed inline ──
    (
        plugin_id = $plugin_id:expr,
        core_version = $core_ver:expr,
        lib_version = $lib_ver:expr,
        plugin_version = $plugin_ver:expr,
        source_descriptors = [ $($source_desc:expr),* $(,)? ],
        reaction_descriptors = [ $($reaction_desc:expr),* $(,)? ],
        bootstrap_descriptors = [ $($bootstrap_desc:expr),* $(,)? ] $(,)?
    ) => {
        fn __auto_create_plugin_vtables() -> (
            Vec<$crate::ffi::SourcePluginVtable>,
            Vec<$crate::ffi::ReactionPluginVtable>,
            Vec<$crate::ffi::BootstrapPluginVtable>,
        ) {
            let source_descs = vec![
                $( $crate::ffi::build_source_plugin_vtable(
                    $source_desc,
                    __plugin_executor,
                    __emit_lifecycle,
                    __plugin_runtime,
                ), )*
            ];
            let reaction_descs = vec![
                $( $crate::ffi::build_reaction_plugin_vtable(
                    $reaction_desc,
                    __plugin_executor,
                    __emit_lifecycle,
                    __plugin_runtime,
                ), )*
            ];
            let bootstrap_descs = vec![
                $( $crate::ffi::build_bootstrap_plugin_vtable(
                    $bootstrap_desc,
                    __plugin_executor,
                    __emit_lifecycle,
                    __plugin_runtime,
                ), )*
            ];
            (source_descs, reaction_descs, bootstrap_descs)
        }

        $crate::export_plugin!(
            @internal
            plugin_id = $plugin_id,
            core_version = $core_ver,
            lib_version = $lib_ver,
            plugin_version = $plugin_ver,
            init_fn = __auto_create_plugin_vtables,
        );
    };

    // ── Internal form: custom init function ──
    (
        @internal
        plugin_id = $plugin_id:expr,
        core_version = $core_ver:expr,
        lib_version = $lib_ver:expr,
        plugin_version = $plugin_ver:expr,
        init_fn = $init_fn:ident $(,)?
    ) => {
        // ── Tokio runtime (accessible to plugin code) ──
        pub fn __plugin_runtime() -> &'static $crate::__tokio::runtime::Runtime {
            use ::std::sync::OnceLock;
            static RT: OnceLock<$crate::__tokio::runtime::Runtime> = OnceLock::new();
            RT.get_or_init(|| {
                $crate::__tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .thread_name(concat!($plugin_id, "-worker"))
                    .build()
                    .expect("Failed to create plugin tokio runtime")
            })
        }

        struct __SendPtr(*mut ::std::ffi::c_void);
        unsafe impl Send for __SendPtr {}

        /// Async executor dispatching to this plugin's tokio runtime.
        pub extern "C" fn __plugin_executor(
            future_ptr: *mut ::std::ffi::c_void,
        ) -> *mut ::std::ffi::c_void {
            let boxed: Box<
                ::std::pin::Pin<
                    Box<dyn ::std::future::Future<Output = *mut ::std::ffi::c_void> + Send>,
                >,
            > = unsafe { Box::from_raw(future_ptr as *mut _) };
            let handle = __plugin_runtime().handle().clone();
            let result = ::std::thread::spawn(move || {
                let raw = handle.block_on(*boxed);
                __SendPtr(raw)
            })
            .join()
            .expect("Plugin executor thread panicked");
            result.0
        }

        /// Run an async future on the plugin runtime, blocking until complete.
        #[allow(dead_code)]
        pub fn plugin_block_on<F>(f: F) -> F::Output
        where
            F: ::std::future::Future + Send + 'static,
            F::Output: Send + 'static,
        {
            let handle = __plugin_runtime().handle().clone();
            ::std::thread::spawn(move || handle.block_on(f))
                .join()
                .expect("plugin_block_on: spawned thread panicked")
        }

        // ── Log/lifecycle callback storage ──
        static __LOG_CB: ::std::sync::atomic::AtomicPtr<()> =
            ::std::sync::atomic::AtomicPtr::new(::std::ptr::null_mut());
        static __LOG_CTX: ::std::sync::atomic::AtomicPtr<::std::ffi::c_void> =
            ::std::sync::atomic::AtomicPtr::new(::std::ptr::null_mut());
        static __LIFECYCLE_CB: ::std::sync::atomic::AtomicPtr<()> =
            ::std::sync::atomic::AtomicPtr::new(::std::ptr::null_mut());
        static __LIFECYCLE_CTX: ::std::sync::atomic::AtomicPtr<::std::ffi::c_void> =
            ::std::sync::atomic::AtomicPtr::new(::std::ptr::null_mut());

        // Note: FfiLogger (log::Log) is no longer used. All log crate events are
        // bridged to tracing via tracing-log's LogTracer, then handled by
        // FfiTracingLayer which has access to span context for correct routing.

        extern "C" fn __set_log_callback_impl(
            ctx: *mut ::std::ffi::c_void,
            callback: $crate::ffi::LogCallbackFn,
        ) {
            __LOG_CTX.store(ctx, ::std::sync::atomic::Ordering::Release);
            __LOG_CB.store(callback as *mut (), ::std::sync::atomic::Ordering::Release);

            // Set up tracing subscriber with LogTracer bridge.
            // LogTracer redirects log crate events → tracing, and FfiTracingLayer
            // forwards all tracing events (including log-bridged ones) through FFI
            // with span context for correct routing.
            $crate::ffi::tracing_bridge::init_tracing_subscriber(
                &__LOG_CB,
                &__LOG_CTX,
                $plugin_id,
            );
        }

        extern "C" fn __set_lifecycle_callback_impl(
            ctx: *mut ::std::ffi::c_void,
            callback: $crate::ffi::LifecycleCallbackFn,
        ) {
            __LIFECYCLE_CTX.store(ctx, ::std::sync::atomic::Ordering::Release);
            __LIFECYCLE_CB.store(
                callback as *mut (),
                ::std::sync::atomic::Ordering::Release,
            );
        }

        /// Emit a lifecycle event to the host.
        pub fn __emit_lifecycle(
            component_id: &str,
            event_type: $crate::ffi::FfiLifecycleEventType,
            message: &str,
        ) {
            let ptr = __LIFECYCLE_CB.load(::std::sync::atomic::Ordering::Acquire);
            if !ptr.is_null() {
                let cb: $crate::ffi::LifecycleCallbackFn =
                    unsafe { ::std::mem::transmute(ptr) };
                let ctx = __LIFECYCLE_CTX.load(::std::sync::atomic::Ordering::Acquire);
                let event = $crate::ffi::FfiLifecycleEvent {
                    component_id: $crate::ffi::FfiStr::from_str(component_id),
                    component_type: $crate::ffi::FfiStr::from_str("plugin"),
                    event_type,
                    message: $crate::ffi::FfiStr::from_str(message),
                    timestamp_us: $crate::ffi::now_us(),
                };
                cb(ctx, &event);
            }
        }

        // ── Plugin metadata ──
        static __PLUGIN_METADATA: $crate::ffi::PluginMetadata = $crate::ffi::PluginMetadata {
            sdk_version: $crate::ffi::FfiStr {
                ptr: $crate::ffi::FFI_SDK_VERSION.as_ptr() as *const ::std::os::raw::c_char,
                len: $crate::ffi::FFI_SDK_VERSION.len(),
            },
            core_version: $crate::ffi::FfiStr {
                ptr: $core_ver.as_ptr() as *const ::std::os::raw::c_char,
                len: $core_ver.len(),
            },
            lib_version: $crate::ffi::FfiStr {
                ptr: $lib_ver.as_ptr() as *const ::std::os::raw::c_char,
                len: $lib_ver.len(),
            },
            plugin_version: $crate::ffi::FfiStr {
                ptr: $plugin_ver.as_ptr() as *const ::std::os::raw::c_char,
                len: $plugin_ver.len(),
            },
            target_triple: $crate::ffi::FfiStr {
                ptr: $crate::ffi::TARGET_TRIPLE.as_ptr() as *const ::std::os::raw::c_char,
                len: $crate::ffi::TARGET_TRIPLE.len(),
            },
        };

        /// Returns plugin metadata for version validation. Called BEFORE init.
        #[no_mangle]
        pub extern "C" fn drasi_plugin_metadata() -> *const $crate::ffi::PluginMetadata {
            &__PLUGIN_METADATA
        }

        /// Plugin entry point. Called AFTER metadata validation passes.
        #[no_mangle]
        pub extern "C" fn drasi_plugin_init() -> *mut $crate::ffi::FfiPluginRegistration {
            match ::std::panic::catch_unwind(|| {
                let _ = __plugin_runtime();
                let (mut source_descs, mut reaction_descs, mut bootstrap_descs) = $init_fn();

                let registration = Box::new($crate::ffi::FfiPluginRegistration {
                    source_plugins: source_descs.as_mut_ptr(),
                    source_plugin_count: source_descs.len(),
                    reaction_plugins: reaction_descs.as_mut_ptr(),
                    reaction_plugin_count: reaction_descs.len(),
                    bootstrap_plugins: bootstrap_descs.as_mut_ptr(),
                    bootstrap_plugin_count: bootstrap_descs.len(),
                    set_log_callback: __set_log_callback_impl,
                    set_lifecycle_callback: __set_lifecycle_callback_impl,
                });
                ::std::mem::forget(source_descs);
                ::std::mem::forget(reaction_descs);
                ::std::mem::forget(bootstrap_descs);
                Box::into_raw(registration)
            }) {
                Ok(ptr) => ptr,
                Err(_) => ::std::ptr::null_mut(),
            }
        }
    };
}
