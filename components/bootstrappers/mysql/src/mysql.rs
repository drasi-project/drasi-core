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

//! MySQL bootstrap handler implementation.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use log::{info, warn};
use mysql_async::prelude::*;
use mysql_async::{Conn, OptsBuilder, Row};
use ordered_float::OrderedFloat;

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_lib::bootstrap::{BootstrapContext, BootstrapRequest};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};

use crate::config::{is_valid_identifier, MySqlBootstrapConfig};

pub struct MySqlBootstrapHandler {
    config: MySqlBootstrapConfig,
    table_keys: HashMap<String, Vec<String>>,
}

impl MySqlBootstrapHandler {
    pub fn new(config: MySqlBootstrapConfig) -> Self {
        let mut map = HashMap::new();
        for key in &config.table_keys {
            map.insert(key.table.clone(), key.key_columns.clone());
        }
        Self {
            config,
            table_keys: map,
        }
    }

    pub async fn execute(
        &mut self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        __settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<usize> {
        info!(
            "Starting MySQL bootstrap for source {} and query {}",
            context.source_id, request.query_id
        );

        self.config.validate()?;

        let mut conn = self.connect().await?;
        let tables = self.determine_tables(&request).await?;
        if tables.is_empty() {
            warn!("No tables selected for bootstrap; check configured allowlist and query labels");
            return Ok(0);
        }

        let mut total = 0usize;
        for (label, table_name) in tables {
            let count = self
                .bootstrap_table(&mut conn, &label, &table_name, context, &event_tx)
                .await?;
            total += count;
        }

        Ok(total)
    }

    async fn connect(&self) -> Result<Conn> {
        let opts = OptsBuilder::default()
            .ip_or_hostname(&self.config.host)
            .tcp_port(self.config.port)
            .user(Some(&self.config.user))
            .pass(Some(&self.config.password))
            .db_name(Some(&self.config.database));
        let conn = Conn::new(opts).await?;
        Ok(conn)
    }

    async fn determine_tables(&self, request: &BootstrapRequest) -> Result<Vec<(String, String)>> {
        let mut tables = Vec::new();
        let allowed: HashSet<String> = self.config.tables.iter().cloned().collect();

        if !request.node_labels.is_empty() {
            for label in &request.node_labels {
                if !allowed.contains(label) {
                    warn!("Requested table '{label}' is not in configured allowlist, skipping");
                    continue;
                }
                if !is_valid_identifier(label) {
                    warn!("Requested table '{label}' has invalid characters, skipping");
                    continue;
                }
                tables.push((label.clone(), label.clone()));
            }
        } else {
            for table in &self.config.tables {
                if !is_valid_identifier(table) {
                    warn!("Configured table '{table}' has invalid characters, skipping");
                    continue;
                }
                tables.push((table.clone(), table.clone()));
            }
        }

        Ok(tables)
    }

    async fn bootstrap_table(
        &self,
        conn: &mut Conn,
        label: &str,
        table_name: &str,
        context: &BootstrapContext,
        event_tx: &BootstrapEventSender,
    ) -> Result<usize> {
        let query = format!("SELECT * FROM {}", quote_identifier(table_name));
        let mut result = conn.query_iter(query).await?;
        let _batch_size = self.config.batch_size;
        let mut total = 0usize;

        while let Some(row) = result.next().await? {
            let source_change = self.row_to_source_change(&row, label, table_name, context)?;
            let event = BootstrapEvent {
                source_id: context.source_id.clone(),
                change: source_change,
                timestamp: Utc::now(),
                sequence: context.next_sequence(),
            };
            event_tx.send(event).await?;
            total += 1;
        }

        info!("Bootstrapped {total} rows from table {table_name}");
        Ok(total)
    }

    fn row_to_source_change(
        &self,
        row: &Row,
        label: &str,
        table_name: &str,
        context: &BootstrapContext,
    ) -> Result<SourceChange> {
        let mut properties = ElementPropertyMap::new();
        let mut key_parts = Vec::new();

        let columns = row.columns_ref();
        for (idx, column) in columns.iter().enumerate() {
            let col_name = column.name_str().to_string();
            let value = match row.as_ref(idx) {
                Some(mysql_async::Value::NULL) => ElementValue::Null,
                Some(mysql_async::Value::Bytes(bytes)) => {
                    ElementValue::String(Arc::from(String::from_utf8_lossy(bytes).as_ref()))
                }
                Some(mysql_async::Value::Int(val)) => ElementValue::Integer(*val),
                Some(mysql_async::Value::UInt(val)) => ElementValue::Integer(*val as i64),
                Some(mysql_async::Value::Float(val)) => {
                    ElementValue::Float(OrderedFloat(*val as f64))
                }
                Some(mysql_async::Value::Double(val)) => ElementValue::Float(OrderedFloat(*val)),
                Some(mysql_async::Value::Date(y, m, d, h, min, s, _)) => ElementValue::String(
                    Arc::from(format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}")),
                ),
                Some(mysql_async::Value::Time(_, days, hours, minutes, seconds, micros)) => {
                    let total_hours = days * 24 + u32::from(*hours);
                    ElementValue::String(Arc::from(format!(
                        "{total_hours:03}:{minutes:02}:{seconds:02}.{micros:06}"
                    )))
                }
                None => ElementValue::Null,
            };

            if let Some(keys) = self.table_keys.get(table_name) {
                if keys.contains(&col_name) {
                    key_parts.push(format_value_for_key(&value));
                }
            }

            properties.insert(&col_name, value);
        }

        if key_parts.is_empty() {
            warn!("No primary key mapping for table '{table_name}', falling back to UUID");
        }

        let element_id = if !key_parts.is_empty() {
            format!("{}:{}", table_name, key_parts.join("_"))
        } else {
            format!("{}:{}", table_name, uuid::Uuid::new_v4())
        };

        let metadata = ElementMetadata {
            reference: ElementReference::new(&context.source_id, &element_id),
            labels: Arc::from(vec![Arc::from(label)]),
            effective_from: Utc::now().timestamp_millis() as u64,
        };

        let element = Element::Node {
            metadata,
            properties,
        };

        Ok(SourceChange::Insert { element })
    }
}

pub(crate) fn escape_identifier(value: &str) -> String {
    value.replace('`', "``")
}

pub(crate) fn quote_identifier(value: &str) -> String {
    format!("`{}`", escape_identifier(value))
}

fn format_value_for_key(value: &ElementValue) -> String {
    match value {
        ElementValue::Null => "null".to_string(),
        ElementValue::Bool(b) => b.to_string(),
        ElementValue::Float(f) => f.to_string(),
        ElementValue::Integer(i) => i.to_string(),
        ElementValue::String(s) => s.to_string(),
        ElementValue::List(l) => l
            .iter()
            .map(format_value_for_key)
            .collect::<Vec<_>>()
            .join("-"),
        ElementValue::Object(_) => "object".to_string(),
    }
}
