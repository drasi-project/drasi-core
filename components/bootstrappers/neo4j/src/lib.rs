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

//! Neo4j bootstrap plugin for Drasi
//!
//! This plugin provides the Neo4j bootstrap provider implementation following
//! the instance-based plugin architecture.
//!
//! # Example
//!
//! ```no_run
//! use drasi_bootstrap_neo4j::Neo4jBootstrapProvider;
//!
//! // Using the builder
//! let provider = Neo4jBootstrapProvider::builder()
//!     .with_uri("bolt://localhost:7687")
//!     .with_user("neo4j")
//!     .with_password("password")
//!     .with_database("neo4j")
//!     .with_labels(vec!["Person".to_string()])
//!     .with_rel_types(vec!["ACTED_IN".to_string()])
//!     .build();
//!
//! // Or using configuration
//! use drasi_bootstrap_neo4j::Neo4jBootstrapConfig;
//!
//! let config = Neo4jBootstrapConfig {
//!     uri: "bolt://localhost:7687".to_string(),
//!     user: "neo4j".to_string(),
//!     password: "password".to_string(),
//!     database: "neo4j".to_string(),
//!     labels: vec!["Person".to_string()],
//!     rel_types: vec!["ACTED_IN".to_string()],
//! };
//! let provider = Neo4jBootstrapProvider::new(config);
//! ```

pub mod config;
pub mod descriptor;
pub mod neo4j;

pub use config::Neo4jBootstrapConfig;
pub use neo4j::{Neo4jBootstrapProvider, Neo4jBootstrapProviderBuilder};

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "neo4j-bootstrap",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [descriptor::Neo4jBootstrapDescriptor],
);
