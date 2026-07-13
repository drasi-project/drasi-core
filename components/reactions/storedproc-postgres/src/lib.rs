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

//! PostgreSQL Stored Procedure reaction plugin for Drasi
//!
//! This plugin implements reactions that invoke PostgreSQL stored procedures when
//! continuous query results change. It supports different procedures for
//! ADD, UPDATE, and DELETE operations using [Handlebars] command templates.
//!
//! Argument values are referenced with the `{{param <expr>}}` helper, which binds
//! them as positional SQL parameters (`$1`, `$2`, …) so untrusted row data can
//! never alter the command structure. The `{{json <expr>}}` helper is also
//! available for embedding serialized JSON into literal SQL text.
//!
//! # Example
//!
//! ```rust,ignore
//! use drasi_reaction_storedproc_postgres::{PostgresStoredProcReaction, QueryConfig, TemplateSpec};
//!
//! let reaction = PostgresStoredProcReaction::builder("user-sync")
//!     .with_connection("localhost", 5432, "mydb", "postgres", "password")
//!     .with_query("user-changes")
//!     .with_default_template(QueryConfig {
//!         added: Some(TemplateSpec::new("CALL add_user({{param after.id}}, {{param after.name}}, {{param after.email}})")),
//!         updated: Some(TemplateSpec::new("CALL update_user({{param after.id}}, {{param after.name}}, {{param after.email}})")),
//!         deleted: Some(TemplateSpec::new("CALL delete_user({{param before.id}})")),
//!     })
//!     .build()
//!     .await?;
//! ```
//!
//! [Handlebars]: https://crates.io/crates/handlebars

pub mod config;
pub mod descriptor;
pub mod executor;
pub mod reaction;
pub(crate) mod render;

pub use config::{PostgresStoredProcReactionConfig, QueryConfig, TemplateSpec};
pub use reaction::PostgresStoredProcReaction;

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "storedproc-postgres-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::PostgresStoredProcReactionDescriptor],
    bootstrap_descriptors = [],
);
