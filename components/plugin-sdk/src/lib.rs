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
//!    [`ReactionPluginDescriptor`](descriptor::ReactionPluginDescriptor),
//!    [`BootstrapPluginDescriptor`](descriptor::BootstrapPluginDescriptor)).
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
pub mod mapper;
pub mod prelude;
pub mod registration;
pub mod resolver;

// Top-level re-exports for convenience
pub use config_value::ConfigValue;
pub use descriptor::{
    BootstrapPluginDescriptor, ReactionPluginDescriptor, SourcePluginDescriptor,
};
pub use mapper::{ConfigMapper, DtoMapper, MappingError};
pub use registration::{PluginRegistration, SDK_VERSION};
pub use resolver::{register_secret_resolver, ResolverError};

/// Export the standard dynamic-plugin entry points.
///
/// This macro generates both:
/// - `drasi_plugin_init` — returns the [`PluginRegistration`] (called by the server)
/// - `drasi_plugin_build_hash` — returns the build hash as a C string (pre-check
///   by the server before calling `drasi_plugin_init`, to catch ABI mismatches early)
///
/// # Usage
///
/// ```rust,ignore
/// use drasi_plugin_sdk::prelude::*;
///
/// export_plugin! {
///     PluginRegistration::new()
///         .with_source(Box::new(MySourceDescriptor))
/// }
/// ```
///
/// The macro is gated on `#[cfg(feature = "dynamic-plugin")]` at the call site,
/// so it only emits symbols when building as a shared library.
#[macro_export]
macro_rules! export_plugin {
    ($registration:expr) => {
        #[no_mangle]
        pub extern "C" fn drasi_plugin_init() -> *mut $crate::PluginRegistration {
            Box::into_raw(Box::new($registration))
        }

        #[no_mangle]
        pub extern "C" fn drasi_plugin_build_hash() -> *const u8 {
            // Return a pointer to a null-terminated C string containing the build hash.
            // The static ensures the pointer remains valid for the lifetime of the process.
            static BUILD_HASH_CSTR: std::sync::LazyLock<std::ffi::CString> =
                std::sync::LazyLock::new(|| {
                    std::ffi::CString::new(drasi_plugin_runtime::BUILD_HASH)
                        .expect("BUILD_HASH contains no interior NUL bytes")
                });
            BUILD_HASH_CSTR.as_ptr() as *const u8
        }
    };
}
