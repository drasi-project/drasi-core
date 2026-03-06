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

//! MySQL source internal types

use std::collections::HashMap;

use mysql_cdc::events::table_map_event::TableMapEvent;

#[derive(Debug, Clone, Default)]
pub struct TableMapping {
    pub tables: HashMap<u64, TableMapEvent>,
}

impl TableMapping {
    pub fn update(&mut self, table_event: TableMapEvent) {
        self.tables.insert(table_event.table_id, table_event);
    }

    pub fn get(&self, table_id: u64) -> Option<&TableMapEvent> {
        self.tables.get(&table_id)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReplicationState {
    pub binlog_file: String,
    pub binlog_position: u32,
    pub gtid_set: Option<String>,
    pub last_processed_timestamp: u64,
}
