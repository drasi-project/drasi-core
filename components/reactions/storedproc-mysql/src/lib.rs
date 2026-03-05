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

//! MySQL Stored Procedure reaction plugin for Drasi
//!
//! This plugin implements reactions that invoke MySQL stored procedures when
//! continuous query results change. It supports different procedures for
//! ADD, UPDATE, and DELETE operations.
//!
//! # Example
//!
//! ```rust,ignore
//! use drasi_reaction_storedproc_mysql::{MySqlStoredProcReaction, QueryConfig, TemplateSpec};
//!
//! let reaction = MySqlStoredProcReaction::builder("user-sync")
//!     .with_hostname("localhost")
//!     .with_port(3306)
//!     .with_database("mydb")
//!     .with_user("root")
//!     .with_password("password")
//!     .with_query("user-changes")
//!     .with_default_template(QueryConfig {
//!         added: Some(TemplateSpec::new("CALL add_user(@after.id, @after.name, @after.email)")),
//!         updated: Some(TemplateSpec::new("CALL update_user(@after.id, @after.name, @after.email)")),
//!         deleted: Some(TemplateSpec::new("CALL delete_user(@before.id)")),
//!     })
//!     .build()?;
//! ```

pub mod config;
pub mod descriptor;
pub mod executor;
pub mod parser;
pub mod reaction;

pub use config::{MySqlStoredProcReactionConfig, QueryConfig, TemplateSpec};
pub use reaction::MySqlStoredProcReaction;

/// Dynamic plugin entry point.
///
/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "storedproc-mysql-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::MySqlStoredProcReactionDescriptor],
    bootstrap_descriptors = [],
);
