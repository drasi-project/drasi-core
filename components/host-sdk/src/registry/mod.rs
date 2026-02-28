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

//! OCI registry support for plugin distribution.
//!
//! This module provides:
//! - Types for plugin metadata, version info, and registry configuration
//! - Platform mapping between OCI platform strings and Rust target triples
//! - OCI client for interacting with container registries
//! - Version resolver for finding compatible plugin versions

pub mod oci;
pub mod platform;
pub mod resolver;
pub mod types;

pub use oci::{OciRegistryClient, PluginSearchResult, PluginVersionInfo};
pub use platform::{oci_platform_to_target_triple, target_triple_to_oci_platform, target_triple_to_arch_suffix, strip_arch_suffix, OciPlatform};
pub use resolver::PluginResolver;
pub use types::{HostVersionInfo, PluginMetadataJson, RegistryAuth, RegistryConfig, ResolvedPlugin};
