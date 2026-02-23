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

//! PostgreSQL Stored Procedure reaction plugin for Drasi
//!
//! This plugin implements reactions that invoke PostgreSQL stored procedures when
//! continuous query results change. It supports different procedures for
//! ADD, UPDATE, and DELETE operations using Handlebars templates.
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

pub use config::{PostgresStoredProcReactionConfig, QueryConfig, TemplateSpec};
pub use reaction::PostgresStoredProcReaction;
