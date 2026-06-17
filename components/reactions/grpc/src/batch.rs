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

use crate::proto::ProtoQueryResultItem;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct BatchKey {
    pub query_id: String,
    metadata: Vec<(String, String)>,
}

impl BatchKey {
    pub(crate) fn new(query_id: impl Into<String>, metadata: HashMap<String, String>) -> Self {
        let mut metadata: Vec<(String, String)> = metadata.into_iter().collect();
        metadata.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        Self {
            query_id: query_id.into(),
            metadata,
        }
    }

    pub(crate) fn metadata_map(&self) -> HashMap<String, String> {
        self.metadata.iter().cloned().collect()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PendingBatch {
    pub key: BatchKey,
    pub items: Vec<ProtoQueryResultItem>,
}

impl PendingBatch {
    pub(crate) fn new(key: BatchKey, item: ProtoQueryResultItem) -> Self {
        Self {
            key,
            items: vec![item],
        }
    }

    pub(crate) fn can_accept(&self, key: &BatchKey) -> bool {
        &self.key == key
    }
}

pub(crate) fn merge_metadata(
    base: &HashMap<String, String>,
    rendered: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut merged = base.clone();
    for (key, value) in rendered {
        merged.insert(key.clone(), value.clone());
    }
    merged
}
