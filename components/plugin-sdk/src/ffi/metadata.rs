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

//! Plugin metadata for version validation at load time.

use super::types::FfiStr;

/// The SDK version of this crate. Used for compatibility checks at plugin load time.
pub const FFI_SDK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The target triple this crate was compiled for.
pub const TARGET_TRIPLE: &str = env!("TARGET_TRIPLE");

/// Metadata returned by `drasi_plugin_metadata()` for version validation.
/// The host checks these fields before calling `drasi_plugin_init()`.
#[repr(C)]
pub struct PluginMetadata {
    /// Version of the drasi-plugin-sdk crate (FFI envelope types).
    pub sdk_version: FfiStr,
    /// Version of drasi-core (opaque pointer types: SourceChange, Element, etc.).
    pub core_version: FfiStr,
    /// Version of drasi-lib (Source/Reaction traits, QueryResult, etc.).
    pub lib_version: FfiStr,
    /// Plugin's own version.
    pub plugin_version: FfiStr,
    /// Target triple (e.g., "x86_64-unknown-linux-gnu").
    pub target_triple: FfiStr,
}
