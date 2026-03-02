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

pub fn relabel_query() -> &'static str {
    "
  MATCH 
      (u:User)
    RETURN
      u.name as userName,
      u.email as userEmail,
      u.role as userRole
    "
}

pub fn middlewares() -> Vec<Arc<SourceMiddlewareConfig>> {
    let cfg: serde_json::Map<String, serde_json::Value> = json!({
        "labelMappings": {
            "Person": "User",
            "Company": "Organization",
            "Employee": "Staff"
        }
    })
    .as_object()
    .unwrap()
    .clone();

    vec![Arc::new(SourceMiddlewareConfig::new(
        "relabel", "relabel", cfg,
    ))]
}

pub fn source_pipeline() -> Vec<String> {
    vec!["relabel".to_string()]
}
