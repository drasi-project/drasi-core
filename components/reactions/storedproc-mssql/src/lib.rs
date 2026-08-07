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

#![allow(unexpected_cfgs)]

//! MS SQL Server Stored Procedure reaction plugin for Drasi
//!
//! This plugin implements reactions that invoke MS SQL Server stored procedures when
//! continuous query results change. It supports different procedures for
//! ADD, UPDATE, and DELETE operations.
//!
//! Commands are Handlebars templates. Row data is passed to a procedure with
//! the `{{param}}` helper, which binds each value as a positional parameter
//! (`@P1`, `@P2`, …) through the driver rather than interpolating it into the
//! SQL text. The `{{json}}` helper serializes a value to a JSON string.
//!
//! # Example
//!
//! ```rust,ignore
//! use drasi_reaction_storedproc_mssql::{MsSqlStoredProcReaction, QueryConfig, TemplateSpec};
//!
//! let default_template = QueryConfig {
//!     added: Some(TemplateSpec::new(
//!         "EXEC add_user {{param after.id}}, {{param after.name}}, {{param after.email}}",
//!     )),
//!     updated: Some(TemplateSpec::new(
//!         "EXEC update_user {{param after.id}}, {{param after.name}}, {{param after.email}}",
//!     )),
//!     deleted: Some(TemplateSpec::new("EXEC delete_user {{param before.id}}")),
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
pub mod reaction;
pub(crate) mod render;

pub use config::{MsSqlStoredProcReactionConfig, QueryConfig, TemplateSpec};
pub use reaction::MsSqlStoredProcReaction;

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
