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

//! MS SQL Server Stored Procedure reaction plugin for Drasi
//!
//! This plugin implements reactions that invoke MS SQL Server stored procedures when
//! continuous query results change. It supports different procedures for
//! ADD, UPDATE, and DELETE operations.
//!
//! # Example
//!
//! ```rust,ignore
//! use drasi_reaction_storedproc_mssql::{MsSqlStoredProcReaction, QueryConfig, TemplateSpec};
//!
//! let default_template = QueryConfig {
//!     added: Some(TemplateSpec {
//!         template: "EXEC add_user @after.id, @after.name, @after.email".to_string(),
//!     }),
//!     updated: Some(TemplateSpec {
//!         template: "EXEC update_user @after.id, @after.name, @after.email".to_string(),
//!     }),
//!     deleted: Some(TemplateSpec {
//!         template: "EXEC delete_user @before.id".to_string(),
//!     }),
//! };
//!
//! let reaction = MsSqlStoredProcReaction::builder("user-sync")
//!     .with_connection("localhost", 1433, "mydb", "sa", "password")
//!     .with_query("user-changes")
//!     .with_default_template(default_template)
//!     .build()?;
//! ```

pub mod config;
pub mod descriptor;
pub mod executor;
pub mod parser;
pub mod reaction;

pub use config::{MsSqlStoredProcReactionConfig, QueryConfig, TemplateSpec};
pub use reaction::MsSqlStoredProcReaction;

/// Dynamic plugin entry point.
///
/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "storedproc-mssql-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::MsSqlStoredProcReactionDescriptor],
    bootstrap_descriptors = [],
);
