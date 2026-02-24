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
pub use resolver::ResolverError;
