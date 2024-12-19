#![allow(clippy::unwrap_used)]
// Copyright 2024 The Drasi Authors.
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

use std::sync::Arc;

use drasi_core::models::SourceMiddlewareConfig;
use serde_json::json;

pub fn unwind_query() -> &'static str {
    "
  MATCH 
      (p:Pod)-[:OWNS]->(c:Container)
    RETURN
      p.metadata.name as pod,
      c.containerID as containerID,
      c.name as name
    "
}

pub fn middlewares() -> Vec<Arc<SourceMiddlewareConfig>> {
    let cfg: serde_json::Map<String, serde_json::Value> = json!({
        "Pod": [{
            "selector": "$.status.containerStatuses[*]",
            "label": "Container",
            "key": "$.containerID",
            "relation": "OWNS"
        }]
    })
    .as_object()
    .unwrap()
    .clone();

    vec![Arc::new(SourceMiddlewareConfig::new(
        "unwind", "unwind", cfg,
    ))]
}

pub fn source_pipeline() -> Vec<String> {
    vec!["unwind".to_string()]
}
