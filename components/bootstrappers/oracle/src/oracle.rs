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

//! Oracle bootstrap provider implementation.

use anyhow::Result;
use async_trait::async_trait;
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::{BootstrapEvent, SourceChangeEvent};
use drasi_oracle_common::{
    extract_row_properties, split_table_name, OracleConnection, OracleSourceConfig,
    PrimaryKeyCache, Scn, SslMode, TableKeyConfig, ORACLE_BOOTSTRAP_SCN_CONTEXT_PROPERTY,
};
use std::sync::Arc;

pub struct OracleBootstrapProvider {
    config: OracleSourceConfig,
}

impl OracleBootstrapProvider {
    pub fn new(config: OracleSourceConfig) -> Self {
        Self { config }
    }

    pub fn builder() -> OracleBootstrapProviderBuilder {
        OracleBootstrapProviderBuilder::new()
    }
}

#[async_trait]
impl BootstrapProvider for OracleBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: drasi_lib::channels::BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<usize> {
        let config = self.config.clone();
        let source_id = context.source_id.clone();
        let snapshot_scn = context
            .get_typed_property::<u64>(ORACLE_BOOTSTRAP_SCN_CONTEXT_PROPERTY)?
            .map(Scn);
        let events = tokio::task::spawn_blocking(move || {
            let mut handler = OracleBootstrapHandler::new(config, source_id, snapshot_scn);
            handler.execute(&request)
        })
        .await??;

        let count = events.len();
        for event in events {
            event_tx
                .send(BootstrapEvent {
                    source_id: event.source_id,
                    change: event.change,
                    timestamp: event.timestamp,
                    sequence: context.next_sequence(),
                })
                .await?;
        }

        Ok(count)
    }
}

pub struct OracleBootstrapProviderBuilder {
    config: OracleSourceConfig,
}

impl OracleBootstrapProviderBuilder {
    pub fn new() -> Self {
        Self {
            config: OracleSourceConfig::default(),
        }
    }

    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.config.host = host.into();
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.config.port = port;
        self
    }

    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.config.database = database.into();
        self
    }

    pub fn with_service(self, service: impl Into<String>) -> Self {
        self.with_database(service)
    }

    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.config.user = user.into();
        self
    }

    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.config.password = password.into();
        self
    }

    pub fn with_tables(mut self, tables: Vec<String>) -> Self {
        self.config.tables = tables;
        self
    }

    pub fn with_table(mut self, table: impl Into<String>) -> Self {
        self.config.tables.push(table.into());
        self
    }

    pub fn with_table_key(mut self, table: impl Into<String>, key_columns: Vec<String>) -> Self {
        self.config.table_keys.push(TableKeyConfig {
            table: table.into(),
            key_columns,
        });
        self
    }

    pub fn with_ssl_mode(mut self, ssl_mode: SslMode) -> Self {
        self.config.ssl_mode = ssl_mode;
        self
    }

    pub fn build(self) -> Result<OracleBootstrapProvider> {
        self.config.validate()?;
        Ok(OracleBootstrapProvider::new(self.config))
    }
}

impl Default for OracleBootstrapProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

struct OracleBootstrapHandler {
    config: OracleSourceConfig,
    source_id: String,
    pk_cache: PrimaryKeyCache,
    snapshot_scn: Option<Scn>,
}

impl OracleBootstrapHandler {
    fn new(config: OracleSourceConfig, source_id: String, snapshot_scn: Option<Scn>) -> Self {
        Self {
            config,
            source_id,
            pk_cache: PrimaryKeyCache::new(),
            snapshot_scn,
        }
    }

    fn execute(&mut self, request: &BootstrapRequest) -> Result<Vec<SourceChangeEvent>> {
        let connection = OracleConnection::connect(&self.config)?;
        let conn = connection.inner();
        self.pk_cache.discover_keys(conn, &self.config)?;

        let tables = self.tables_for_request(request);
        let mut events = Vec::new();

        for table in tables {
            let (schema, table_name) = split_table_name(&table, &self.config.user)?;
            let query = if let Some(snapshot_scn) = self.snapshot_scn {
                format!(
                    "SELECT * FROM \"{schema}\".\"{table_name}\" AS OF SCN {}",
                    snapshot_scn.0
                )
            } else {
                format!("SELECT * FROM \"{schema}\".\"{table_name}\"")
            };
            let rows = conn.query(&query, &[])?;
            for row in rows {
                let row = row?;
                let properties = extract_row_properties(&row)?;
                let element_id =
                    self.pk_cache
                        .make_element_id(&schema, &table_name, &properties)?;
                let timestamp = chrono::Utc::now();
                let effective_from = timestamp.timestamp_millis() as u64;
                let element = Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new(&self.source_id, &element_id),
                        labels: Arc::from([Arc::from(table_name.to_lowercase())]),
                        effective_from,
                    },
                    properties,
                };
                events.push(SourceChangeEvent {
                    source_id: self.source_id.clone(),
                    change: SourceChange::Insert { element },
                    timestamp,
                });
            }
        }

        Ok(events)
    }

    fn tables_for_request(&self, request: &BootstrapRequest) -> Vec<String> {
        if request.node_labels.is_empty() {
            return self.config.tables.clone();
        }

        let labels = request
            .node_labels
            .iter()
            .map(|label| label.to_lowercase())
            .collect::<Vec<_>>();

        self.config
            .tables
            .iter()
            .filter(|table| {
                let (_, table_name) =
                    split_table_name(table, &self.config.user).expect("validated table name");
                labels
                    .iter()
                    .any(|label| label == &table_name.to_lowercase())
            })
            .cloned()
            .collect()
    }
}
