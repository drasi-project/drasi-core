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

//! Plugin descriptor traits that define how plugins advertise their capabilities.
//!
//! Each plugin type (source, reaction, bootstrapper) has a corresponding descriptor
//! trait. A plugin crate implements one or more of these traits and returns instances
//! via [`PluginRegistration`](crate::registration::PluginRegistration).
//!
//! # Descriptor Responsibilities
//!
//! Each descriptor provides:
//!
//! 1. **Kind** — A unique string identifier (e.g., `"postgres"`, `"http"`, `"log"`).
//! 2. **Config version** — A semver string for the plugin's DTO version.
//! 3. **Config schema** — A serialized [utoipa](https://docs.rs/utoipa) `Schema` object
//!    (as JSON) that describes the plugin's configuration DTO. This is used by the server
//!    to generate the OpenAPI specification.
//! 4. **Factory method** — An async `create_*` method that takes raw JSON config and
//!    returns a configured plugin instance.
//!
//! # Schema Generation
//!
//! Plugins generate their schema by deriving [`utoipa::ToSchema`] on their DTO struct
//! and serializing it to JSON:
//!
//! ```rust,ignore
//! use utoipa::openapi::schema::Schema;
//! use drasi_plugin_sdk::prelude::*;
//!
//! #[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
//! #[serde(rename_all = "camelCase")]
//! pub struct MySourceConfigDto {
//!     #[schema(value_type = ConfigValueString)]
//!     pub host: ConfigValue<String>,
//!     #[schema(value_type = ConfigValueU16)]
//!     pub port: ConfigValue<u16>,
//! }
//!
//! // In the descriptor implementation:
//! fn config_schema_json(&self) -> String {
//!     let schema = <MySourceConfigDto as utoipa::ToSchema>::schema();
//!     serde_json::to_string(&schema).unwrap()
//! }
//! ```
//!
//! # DTO Versioning
//!
//! Each plugin versions its DTO independently using semver:
//!
//! - **Major** version bump → Breaking change (field removed, type changed, renamed).
//! - **Minor** version bump → Additive change (new optional field added).
//! - **Patch** version bump → Documentation or description change.
//!
//! The server compares the plugin's `config_version()` against known versions and
//! can reject incompatible plugins at load time.
//!
//! # Dynamic Loading
//!
//! Descriptors are fully compatible with dynamic loading. When a plugin is compiled
//! as a `cdylib` shared library, the descriptor trait objects are passed to the server
//! through the [`PluginRegistration`](crate::registration::PluginRegistration) returned
//! by the `drasi_plugin_init()` entry point. The server calls the descriptor methods
//! (e.g., `kind()`, `config_schema_json()`, `create_source()`) across the shared library
//! boundary. Both plugin and server **must** be compiled with the same Rust toolchain
//! and the same `drasi-plugin-sdk` version for this to work correctly.
//!
//! # Complete Example
//!
//! ```rust,ignore
//! use drasi_plugin_sdk::prelude::*;
//! use drasi_lib::sources::Source;
//!
//! /// Descriptor for the PostgreSQL source plugin.
//! pub struct PostgresSourceDescriptor;
//!
//! #[async_trait]
//! impl SourcePluginDescriptor for PostgresSourceDescriptor {
//!     fn kind(&self) -> &str {
//!         "postgres"
//!     }
//!
//!     fn config_version(&self) -> &str {
//!         "1.0.0"
//!     }
//!
//!     fn config_schema_json(&self) -> String {
//!         let schema = <PostgresSourceConfigDto as utoipa::ToSchema>::schema();
//!         serde_json::to_string(&schema).unwrap()
//!     }
//!
//!     async fn create_source(
//!         &self,
//!         id: &str,
//!         config_json: &serde_json::Value,
//!         auto_start: bool,
//!     ) -> anyhow::Result<Box<dyn Source>> {
//!         let dto: PostgresSourceConfigDto = serde_json::from_value(config_json.clone())?;
//!         let mapper = DtoMapper::new();
//!         let host = mapper.resolve_string(&dto.host)?;
//!         let port = mapper.resolve_typed(&dto.port)?;
//!         // ... build and return the source
//!         todo!()
//!     }
//! }
//! ```

use async_trait::async_trait;
use drasi_lib::bootstrap::BootstrapProvider;
use drasi_lib::reactions::Reaction;
use drasi_lib::sources::Source;

/// Descriptor for a **source** plugin.
///
/// Source plugins ingest data from external systems (databases, APIs, message queues)
/// and feed change events into the Drasi query engine.
///
/// # Implementors
///
/// Each source plugin crate (e.g., `drasi-source-postgres`) implements this trait
/// on a zero-sized descriptor struct and returns it via [`PluginRegistration`].
///
/// See the [module docs](self) for a complete example.
#[async_trait]
pub trait SourcePluginDescriptor: Send + Sync {
    /// The unique kind identifier for this source (e.g., `"postgres"`, `"http"`, `"mock"`).
    ///
    /// This value is used as the `kind` field in YAML configuration and API requests.
    /// Must be lowercase, alphanumeric with hyphens (e.g., `"my-source"`).
    fn kind(&self) -> &str;

    /// The semver version of this plugin's configuration DTO.
    ///
    /// Bump major for breaking changes, minor for new optional fields, patch for docs.
    fn config_version(&self) -> &str;

    /// Returns all OpenAPI schemas for this plugin as a JSON-serialized map.
    ///
    /// The return value is a JSON object where keys are schema names and values
    /// are utoipa `Schema` objects. This must include the top-level config DTO
    /// (identified by [`config_schema_name()`](Self::config_schema_name)) as well
    /// as any nested types it references.
    ///
    /// # Implementation
    ///
    /// Use `#[derive(OpenApi)]` listing only the top-level DTO to automatically
    /// collect all transitive schema dependencies:
    ///
    /// ```rust,ignore
    /// use utoipa::OpenApi;
    ///
    /// #[derive(OpenApi)]
    /// #[openapi(schemas(MyConfigDto))]
    /// struct MyPluginSchemas;
    ///
    /// fn config_schema_json(&self) -> String {
    ///     let api = MyPluginSchemas::openapi();
    ///     serde_json::to_string(&api.components.as_ref().unwrap().schemas).unwrap()
    /// }
    /// ```
    fn config_schema_json(&self) -> String;

    /// Returns the OpenAPI schema name for this plugin's configuration DTO.
    ///
    /// This name is used as the key in the OpenAPI `components/schemas` map.
    /// It should match the `#[schema(as = ...)]` annotation on the DTO, or the
    /// struct name if no alias is set.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// fn config_schema_name(&self) -> &str {
    ///     "PostgresSourceConfig"
    /// }
    /// ```
    fn config_schema_name(&self) -> &str;

    /// Create a new source instance from the given configuration.
    ///
    /// # Arguments
    ///
    /// - `id` — The unique identifier for this source instance.
    /// - `config_json` — The plugin-specific configuration as a JSON value.
    ///   This should be deserialized into the plugin's DTO type.
    /// - `auto_start` — Whether the source should start automatically after creation.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration is invalid or the source cannot be created.
    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Source>>;
}

/// Descriptor for a **reaction** plugin.
///
/// Reaction plugins consume query results and perform side effects (webhooks,
/// logging, stored procedures, SSE streams, etc.).
///
/// # Implementors
///
/// Each reaction plugin crate (e.g., `drasi-reaction-http`) implements this trait
/// on a zero-sized descriptor struct and returns it via [`PluginRegistration`].
#[async_trait]
pub trait ReactionPluginDescriptor: Send + Sync {
    /// The unique kind identifier for this reaction (e.g., `"http"`, `"log"`, `"sse"`).
    fn kind(&self) -> &str;

    /// The semver version of this plugin's configuration DTO.
    fn config_version(&self) -> &str;

    /// Returns all OpenAPI schemas as a JSON-serialized map (see [`SourcePluginDescriptor::config_schema_json`]).
    fn config_schema_json(&self) -> String;

    /// Returns the OpenAPI schema name for this plugin's configuration DTO.
    fn config_schema_name(&self) -> &str;

    /// Create a new reaction instance from the given configuration.
    ///
    /// # Arguments
    ///
    /// - `id` — The unique identifier for this reaction instance.
    /// - `query_ids` — The IDs of queries this reaction subscribes to.
    /// - `config_json` — The plugin-specific configuration as a JSON value.
    /// - `auto_start` — Whether the reaction should start automatically after creation.
    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>>;
}

/// Descriptor for a **bootstrap** plugin.
///
/// Bootstrap plugins provide initial data snapshots for sources when queries
/// first subscribe. They deliver historical/current state so queries start with
/// a complete view of the data.
///
/// # Implementors
///
/// Each bootstrap plugin crate (e.g., `drasi-bootstrap-postgres`) implements this trait
/// on a zero-sized descriptor struct and returns it via [`PluginRegistration`].
#[async_trait]
pub trait BootstrapPluginDescriptor: Send + Sync {
    /// The unique kind identifier for this bootstrapper (e.g., `"postgres"`, `"scriptfile"`).
    fn kind(&self) -> &str;

    /// The semver version of this plugin's configuration DTO.
    fn config_version(&self) -> &str;

    /// Returns all OpenAPI schemas as a JSON-serialized map (see [`SourcePluginDescriptor::config_schema_json`]).
    fn config_schema_json(&self) -> String;

    /// Returns the OpenAPI schema name for this plugin's configuration DTO.
    fn config_schema_name(&self) -> &str;

    /// Create a new bootstrap provider from the given configuration.
    ///
    /// # Arguments
    ///
    /// - `config_json` — The bootstrap-specific configuration as a JSON value.
    /// - `source_config_json` — The parent source's configuration, which the
    ///   bootstrapper may need to connect to the same data system.
    async fn create_bootstrap_provider(
        &self,
        config_json: &serde_json::Value,
        source_config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn BootstrapProvider>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::SubscriptionResponse;

    // A minimal mock source for testing the descriptor trait
    struct MockTestSource {
        id: String,
    }

    #[async_trait]
    impl Source for MockTestSource {
        fn id(&self) -> &str {
            &self.id
        }
        fn type_name(&self) -> &str {
            "test"
        }
        fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
            std::collections::HashMap::new()
        }
        async fn start(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn status(&self) -> drasi_lib::ComponentStatus {
            drasi_lib::ComponentStatus::Stopped
        }
        async fn subscribe(
            &self,
            _settings: drasi_lib::config::SourceSubscriptionSettings,
        ) -> anyhow::Result<SubscriptionResponse> {
            unimplemented!()
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        async fn initialize(&self, _context: drasi_lib::SourceRuntimeContext) {}
    }

    struct TestSourceDescriptor;

    #[async_trait]
    impl SourcePluginDescriptor for TestSourceDescriptor {
        fn kind(&self) -> &str {
            "test"
        }
        fn config_version(&self) -> &str {
            "1.0.0"
        }
        fn config_schema_json(&self) -> String {
            r#"{"TestSourceConfig":{"type":"object"}}"#.to_string()
        }
        fn config_schema_name(&self) -> &str {
            "TestSourceConfig"
        }
        async fn create_source(
            &self,
            id: &str,
            _config_json: &serde_json::Value,
            _auto_start: bool,
        ) -> anyhow::Result<Box<dyn Source>> {
            Ok(Box::new(MockTestSource { id: id.to_string() }))
        }
    }

    #[tokio::test]
    async fn test_source_descriptor_kind() {
        let desc = TestSourceDescriptor;
        assert_eq!(desc.kind(), "test");
    }

    #[tokio::test]
    async fn test_source_descriptor_version() {
        let desc = TestSourceDescriptor;
        assert_eq!(desc.config_version(), "1.0.0");
    }

    #[tokio::test]
    async fn test_source_descriptor_schema() {
        let desc = TestSourceDescriptor;
        let schema = desc.config_schema_json();
        let parsed: serde_json::Value = serde_json::from_str(&schema).expect("valid JSON");
        assert_eq!(parsed["TestSourceConfig"]["type"], "object");
    }

    #[tokio::test]
    async fn test_source_descriptor_create() {
        let desc = TestSourceDescriptor;
        let config = serde_json::json!({});
        let source = desc
            .create_source("my-source", &config, true)
            .await
            .expect("create source");
        assert_eq!(source.id(), "my-source");
    }
}
