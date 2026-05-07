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
use mysql_common::constants::ColumnType;
use ordered_float::OrderedFloat;

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_lib::bootstrap::{BootstrapContext, BootstrapRequest, BootstrapResult};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};

use crate::config::MySqlBootstrapConfig;
use drasi_mysql_common::{
    escape_identifier, format_value_for_key, is_valid_identifier, quote_identifier,
};

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
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        info!(
            "Starting MySQL bootstrap for source {} and query {}",
            context.source_id, request.query_id
        );

        self.config.validate()?;

        let mut conn = self.connect().await?;
        let tables = self.determine_tables(&request).await?;
        if tables.is_empty() {
            warn!("No tables selected for bootstrap; check configured allowlist and query labels");
            return Ok(BootstrapResult {
                event_count: 0,
                last_sequence: None,
                sequences_aligned: false,
            });
        }

        let mut total = 0usize;
        for (label, table_name) in tables {
            let count = self
                .bootstrap_table(&mut conn, &label, &table_name, context, &event_tx)
                .await?;
            total += count;
        }

        Ok(BootstrapResult {
            event_count: total,
            last_sequence: None,
            sequences_aligned: false,
        })
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
            let value = self.convert_column_value(row, idx, column.column_type());

            if let Some(keys) = self.table_keys.get(table_name) {
                if keys.contains(&col_name) {
                    key_parts.push(format_value_for_key(&value));
                }
            } else if col_name.eq_ignore_ascii_case("id") {
                key_parts.push(format_value_for_key(&value));
            }

            properties.insert(&col_name, value);
        }

        if key_parts.is_empty() {
            anyhow::bail!(
                "Cannot construct a deterministic element ID for table '{table_name}': \
                 no key columns configured and no 'id' column found. \
                 Configure key_columns for this table."
            );
        }

        let element_id = format!("{}:{}", table_name, key_parts.join("_"));

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

    /// Converts a MySQL row value to an ElementValue using column type metadata.
    ///
    /// The text protocol returns all values as `Value::Bytes`. We use the column
    /// type to properly parse integers, floats, dates, etc. so that bootstrap
    /// and CDC produce identical type mappings.
    fn convert_column_value(&self, row: &Row, idx: usize, col_type: ColumnType) -> ElementValue {
        match row.as_ref(idx) {
            None | Some(mysql_async::Value::NULL) => ElementValue::Null,
            Some(mysql_async::Value::Int(val)) => ElementValue::Integer(*val),
            Some(mysql_async::Value::UInt(val)) => {
                if *val <= i64::MAX as u64 {
                    ElementValue::Integer(*val as i64)
                } else {
                    ElementValue::String(Arc::from(val.to_string()))
                }
            }
            Some(mysql_async::Value::Float(val)) => ElementValue::Float(OrderedFloat(*val as f64)),
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
            Some(mysql_async::Value::Bytes(bytes)) => {
                let text = String::from_utf8_lossy(bytes);
                match col_type {
                    // Integer types
                    ColumnType::MYSQL_TYPE_TINY
                    | ColumnType::MYSQL_TYPE_SHORT
                    | ColumnType::MYSQL_TYPE_LONG
                    | ColumnType::MYSQL_TYPE_LONGLONG
                    | ColumnType::MYSQL_TYPE_INT24
                    | ColumnType::MYSQL_TYPE_YEAR => {
                        if let Ok(val) = text.parse::<i64>() {
                            ElementValue::Integer(val)
                        } else if let Ok(val) = text.parse::<u64>() {
                            if val <= i64::MAX as u64 {
                                ElementValue::Integer(val as i64)
                            } else {
                                ElementValue::String(Arc::from(text.into_owned()))
                            }
                        } else {
                            ElementValue::String(Arc::from(text.into_owned()))
                        }
                    }
                    // Float types
                    ColumnType::MYSQL_TYPE_FLOAT => {
                        if let Ok(val) = text.parse::<f32>() {
                            ElementValue::Float(OrderedFloat(val as f64))
                        } else {
                            ElementValue::String(Arc::from(text.into_owned()))
                        }
                    }
                    ColumnType::MYSQL_TYPE_DOUBLE => {
                        if let Ok(val) = text.parse::<f64>() {
                            ElementValue::Float(OrderedFloat(val))
                        } else {
                            ElementValue::String(Arc::from(text.into_owned()))
                        }
                    }
                    // Date/time types — keep as formatted strings
                    ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => {
                        // CDC formats dates as "YYYY-MM-DD HH:MM:SS", add time component
                        ElementValue::String(Arc::from(format!("{text} 00:00:00")))
                    }
                    ColumnType::MYSQL_TYPE_TIME | ColumnType::MYSQL_TYPE_TIME2 => {
                        // CDC formats time as "HHH:MM:SS.micros", normalize
                        let parts: Vec<&str> = text.splitn(2, '.').collect();
                        let time_part = parts[0];
                        let micros = parts.get(1).unwrap_or(&"000000");
                        let hms: Vec<&str> = time_part.split(':').collect();
                        if hms.len() == 3 {
                            let h: u32 = hms[0].parse().unwrap_or(0);
                            let m: u32 = hms[1].parse().unwrap_or(0);
                            let s: u32 = hms[2].parse().unwrap_or(0);
                            let micros_val: u32 = micros.parse().unwrap_or(0);
                            ElementValue::String(Arc::from(format!(
                                "{h:03}:{m:02}:{s:02}.{micros_val:06}"
                            )))
                        } else {
                            ElementValue::String(Arc::from(text.into_owned()))
                        }
                    }
                    ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_DATETIME2 => {
                        ElementValue::String(Arc::from(text.into_owned()))
                    }
                    ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2 => {
                        ElementValue::String(Arc::from(text.into_owned()))
                    }
                    // All other types (VARCHAR, DECIMAL, ENUM, SET, BLOB, etc.) as strings
                    _ => ElementValue::String(Arc::from(text.into_owned())),
                }
            }
        }
    }
}
