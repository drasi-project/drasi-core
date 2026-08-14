#![allow(unexpected_cfgs)]
// Copyright 2026 The Drasi Authors.
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

//! GitHub source plugin for Drasi.
//!
//! This source receives signed GitHub webhooks, durably persists admitted
//! deliveries to the Drasi WAL, then hydrates authoritative object state from
//! the GitHub GraphQL API and emits normalized `SourceChange` events.

pub mod config;
pub mod descriptor;
pub mod source;

mod bootstrap;
mod graphql;
mod hydrator;
mod mapping;
mod rate_limit;
mod reconciler;
mod types;
mod webhook;

pub use config::GitHubSourceConfig;
pub use source::{GitHubSource, GitHubSourceBuilder};

#[cfg(test)]
mod tests;

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "github-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::GitHubSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
