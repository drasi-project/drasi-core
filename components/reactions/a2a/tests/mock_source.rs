// Copyright 2026 The Drasi Authors.
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

use std::collections::HashMap;

use chrono::Utc;
use drasi_lib::channels::{QueryResult, ResultDiff};
use serde_json::Value;

pub fn add_result(query_id: &str, sequence: u64, after: Value) -> QueryResult {
    QueryResult::new(
        query_id.to_string(),
        sequence,
        Utc::now(),
        vec![ResultDiff::Add {
            data: after,
            row_signature: 0,
        }],
        HashMap::new(),
    )
}

pub fn update_result(query_id: &str, sequence: u64, before: Value, after: Value) -> QueryResult {
    QueryResult::new(
        query_id.to_string(),
        sequence,
        Utc::now(),
        vec![ResultDiff::Update {
            data: serde_json::json!({}),
            before,
            after,
            grouping_keys: None,
            row_signature: 0,
        }],
        HashMap::new(),
    )
}

pub fn delete_result(query_id: &str, sequence: u64, before: Value) -> QueryResult {
    QueryResult::new(
        query_id.to_string(),
        sequence,
        Utc::now(),
        vec![ResultDiff::Delete {
            data: before,
            row_signature: 0,
        }],
        HashMap::new(),
    )
}
