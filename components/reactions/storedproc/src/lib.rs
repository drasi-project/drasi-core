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

//! Stored Procedure reaction plugin for Drasi
//!
//! This plugin implements reactions that invoke SQL stored procedures when
//! continuous query results change. It supports different procedures for
//! ADD, UPDATE, and DELETE operations.
//!
//! # Example
//!
//! ```rust,ignore
//! use drasi_reaction_storedproc::{StoredProcReaction, DatabaseClient};
//!
//! let reaction = StoredProcReaction::builder("user-sync")
//!     .with_database_client(DatabaseClient::PostgreSQL)
//!     .with_connection("localhost", 5432, "mydb", "postgres", "password")
//!     .with_query("user-changes")
//!     .with_added_command("CALL add_user(@id, @name, @email)")
//!     .with_updated_command("CALL update_user(@id, @name, @email)")
//!     .with_deleted_command("CALL delete_user(@id)")
//!     .build()?;
//! ```

pub mod config;
pub mod executor;
pub mod parser;
pub mod storedproc;

pub use config::{DatabaseClient, StoredProcReactionConfig};
pub use storedproc::StoredProcReaction;

/// Builder for StoredProcReaction
pub struct StoredProcReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: StoredProcReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl StoredProcReactionBuilder {
    /// Create a new builder with the given reaction ID
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: StoredProcReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    /// Set the database client type
    pub fn with_database_client(mut self, client: DatabaseClient) -> Self {
        self.config.database_client = client;
        self
    }

    /// Set the database connection parameters
    pub fn with_connection(
        mut self,
        hostname: impl Into<String>,
        port: u16,
        database: impl Into<String>,
        user: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.config.hostname = hostname.into();
        self.config.port = port;
        self.config.database = database.into();
        self.config.user = user.into();
        self.config.password = password.into();
        self
    }

    /// Set the database hostname
    pub fn with_hostname(mut self, hostname: impl Into<String>) -> Self {
        self.config.hostname = hostname.into();
        self
    }

    /// Set the database port
    pub fn with_port(mut self, port: u16) -> Self {
        self.config.port = port;
        self
    }

    /// Set the database name
    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.config.database = database.into();
        self
    }

    /// Set the database user
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.config.user = user.into();
        self
    }

    /// Set the database password
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.config.password = password.into();
        self
    }

    /// Enable or disable SSL/TLS
    pub fn with_ssl(mut self, enable: bool) -> Self {
        self.config.ssl = enable;
        self
    }

    /// Set the stored procedure command for added results
    ///
    /// Use @fieldName syntax to reference query result fields.
    /// Example: "CALL add_user(@id, @name, @email)"
    pub fn with_added_command(mut self, command: impl Into<String>) -> Self {
        self.config.added_command = Some(command.into());
        self
    }

    /// Set the stored procedure command for updated results
    ///
    /// Use @fieldName syntax to reference query result fields.
    /// Example: "CALL update_user(@id, @name, @email)"
    pub fn with_updated_command(mut self, command: impl Into<String>) -> Self {
        self.config.updated_command = Some(command.into());
        self
    }

    /// Set the stored procedure command for deleted results
    ///
    /// Use @fieldName syntax to reference query result fields.
    /// Example: "CALL delete_user(@id)"
    pub fn with_deleted_command(mut self, command: impl Into<String>) -> Self {
        self.config.deleted_command = Some(command.into());
        self
    }

    /// Add a query to subscribe to
    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Set all queries to subscribe to
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    /// Set the connection pool size
    pub fn with_connection_pool_size(mut self, size: usize) -> Self {
        self.config.connection_pool_size = size;
        self
    }

    /// Set the command timeout in milliseconds
    pub fn with_command_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.config.command_timeout_ms = timeout_ms;
        self
    }

    /// Set the number of retry attempts on failure
    pub fn with_retry_attempts(mut self, attempts: u32) -> Self {
        self.config.retry_attempts = attempts;
        self
    }

    /// Set the priority queue capacity
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set whether the reaction should auto-start
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set the full configuration at once
    pub fn with_config(mut self, config: StoredProcReactionConfig) -> Self {
        self.config = config;
        self
    }

    /// Build the StoredProcReaction
    pub async fn build(self) -> anyhow::Result<StoredProcReaction> {
        StoredProcReaction::from_builder(
            self.id,
            self.queries,
            self.config,
            self.priority_queue_capacity,
            self.auto_start,
        )
        .await
    }
}

#[cfg(test)]
mod tests;
