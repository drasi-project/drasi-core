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

//! HTTP bootstrap plugin for Drasi
//!
//! This plugin fetches initial state from REST APIs to populate graph queries.
//! It supports multiple endpoints, various authentication methods, pagination
//! strategies, and Handlebars-based response-to-element mapping.

pub mod auth;
pub mod config;
pub mod content_parser;
pub mod descriptor;
pub mod pagination;
pub mod provider;
pub mod response;
pub mod template_engine;

pub use config::HttpBootstrapConfig;
pub use provider::HttpBootstrapProvider;

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "http-bootstrap",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [descriptor::HttpBootstrapDescriptor],
);
