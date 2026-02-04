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

//! MySQL bootstrap provider for Drasi.
//!
//! Provides an initial snapshot of MySQL tables when a query subscribes
//! with bootstrap enabled.

mod config;
mod mysql;
#[cfg(test)]
mod tests;

pub use config::{MySqlBootstrapConfig, TableKeyConfig};

use async_trait::async_trait;
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::BootstrapEventSender;

#[derive(Clone)]
pub struct MySqlBootstrapProvider {
    config: MySqlBootstrapConfig,
}

impl MySqlBootstrapProvider {
    pub fn new(config: MySqlBootstrapConfig) -> Self {
        Self { config }
    }

    pub fn builder() -> MySqlBootstrapBuilder {
        MySqlBootstrapBuilder::new()
    }
}

#[async_trait]
impl BootstrapProvider for MySqlBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> anyhow::Result<usize> {
        let mut handler = mysql::MySqlBootstrapHandler::new(self.config.clone());
        handler.execute(request, context, event_tx, settings).await
    }
}

/// Builder for MySQL bootstrap provider configuration.
///
/// Provides a fluent API for constructing MySQL bootstrap configurations.
pub struct MySqlBootstrapBuilder {
    host: String,
    port: u16,
    database: String,
    user: String,
    password: String,
    tables: Vec<String>,
    table_keys: Vec<TableKeyConfig>,
    batch_size: usize,
}

impl Default for MySqlBootstrapBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl MySqlBootstrapBuilder {
    pub fn new() -> Self {
        Self {
            host: "localhost".to_string(),
            port: 3306,
            database: String::new(),
            user: String::new(),
            password: String::new(),
            tables: Vec::new(),
            table_keys: Vec::new(),
            batch_size: 1000,
        }
    }

    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.database = database.into();
        self
    }

    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = user.into();
        self
    }

    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = password.into();
        self
    }

    pub fn with_tables(mut self, tables: Vec<String>) -> Self {
        self.tables = tables;
        self
    }

    pub fn add_table(mut self, table: impl Into<String>) -> Self {
        self.tables.push(table.into());
        self
    }

    pub fn with_table_keys(mut self, table_keys: Vec<TableKeyConfig>) -> Self {
        self.table_keys = table_keys;
        self
    }

    pub fn add_table_key(mut self, table_key: TableKeyConfig) -> Self {
        self.table_keys.push(table_key);
        self
    }

    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size;
        self
    }

    pub fn build(self) -> anyhow::Result<MySqlBootstrapProvider> {
        let config = MySqlBootstrapConfig {
            host: self.host,
            port: self.port,
            database: self.database,
            user: self.user,
            password: self.password,
            tables: self.tables,
            table_keys: self.table_keys,
            batch_size: self.batch_size,
        };
        config.validate()?;
        Ok(MySqlBootstrapProvider::new(config))
    }
}
