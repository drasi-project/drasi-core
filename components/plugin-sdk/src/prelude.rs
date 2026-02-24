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

//! Convenience re-exports for plugin authors.
//!
//! Import everything you need with a single `use` statement:
//!
//! ```rust,ignore
//! use drasi_plugin_sdk::prelude::*;
//! ```
//!
//! This re-exports:
//!
//! - Configuration types: [`ConfigValue`], type aliases, schema wrappers
//! - Mapping infrastructure: [`DtoMapper`], [`ConfigMapper`], [`MappingError`]
//! - Resolver types: [`ValueResolver`], [`ResolverError`]
//! - Descriptor traits: [`SourcePluginDescriptor`], [`ReactionPluginDescriptor`],
//!   [`BootstrapPluginDescriptor`]
//! - Registration: [`PluginRegistration`]
//! - Common re-exports: [`async_trait`], [`serde`], [`utoipa`]

// Configuration value types
pub use crate::config_value::{
    ConfigValue, ConfigValueBool, ConfigValueBoolSchema, ConfigValueString,
    ConfigValueStringSchema, ConfigValueU16, ConfigValueU16Schema, ConfigValueU32,
    ConfigValueU32Schema, ConfigValueU64, ConfigValueU64Schema, ConfigValueUsize,
    ConfigValueUsizeSchema,
};

// Mapping infrastructure
pub use crate::mapper::{ConfigMapper, DtoMapper, MappingError};

// Resolver types
pub use crate::resolver::{
    EnvironmentVariableResolver, ResolverError, SecretResolver, ValueResolver,
};

// Plugin descriptor traits
pub use crate::descriptor::{
    BootstrapPluginDescriptor, ReactionPluginDescriptor, SourcePluginDescriptor,
};

// Registration
pub use crate::registration::PluginRegistration;

// Common re-exports that plugin authors need
pub use async_trait::async_trait;
pub use serde::{Deserialize, Serialize};
