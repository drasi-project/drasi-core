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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::drasi_v1::QueryResultItemType;

    fn meta(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn item() -> ProtoQueryResultItem {
        ProtoQueryResultItem {
            item_type: QueryResultItemType::Add as i32,
            row_signature: 1,
            before: None,
            after: None,
            sequence: 0,
            timestamp: None,
            metadata: None,
            payload: None,
        }
    }

    #[test]
    fn batch_key_equality_ignores_metadata_insertion_order() {
        // BatchKey sorts metadata internally so two keys with the same logical
        // content compare (and hash) equal regardless of insertion order —
        // this is what lets the runners coalesce items into one batch.
        let a = BatchKey::new("q", meta(&[("x", "1"), ("y", "2")]));
        let b = BatchKey::new("q", meta(&[("y", "2"), ("x", "1")]));
        assert_eq!(a, b);

        let mut set = std::collections::HashSet::new();
        set.insert(a);
        assert!(set.contains(&b), "equal keys must hash to the same bucket");
    }

    #[test]
    fn batch_key_differs_on_query_id_or_metadata() {
        let base = BatchKey::new("q", meta(&[("x", "1")]));
        assert_ne!(base, BatchKey::new("other", meta(&[("x", "1")])));
        assert_ne!(base, BatchKey::new("q", meta(&[("x", "2")])));
        assert_ne!(base, BatchKey::new("q", HashMap::new()));
    }

    #[test]
    fn metadata_map_round_trips_the_normalized_entries() {
        let key = BatchKey::new("q", meta(&[("b", "2"), ("a", "1")]));
        assert_eq!(key.metadata_map(), meta(&[("a", "1"), ("b", "2")]));
    }

    #[test]
    fn pending_batch_starts_with_one_item_and_accepts_matching_key() {
        let key = BatchKey::new("q", meta(&[("x", "1")]));
        let batch = PendingBatch::new(key.clone(), item());
        assert_eq!(batch.items.len(), 1);
        assert!(batch.can_accept(&key));
        assert!(batch.can_accept(&BatchKey::new("q", meta(&[("x", "1")]))));
        assert!(!batch.can_accept(&BatchKey::new("q", meta(&[("x", "2")]))));
    }

    #[test]
    fn merge_metadata_lets_rendered_entries_override_base() {
        let base = meta(&[("a", "base"), ("shared", "base")]);
        let rendered = meta(&[("shared", "rendered"), ("b", "rendered")]);
        let merged = merge_metadata(&base, &rendered);
        assert_eq!(merged.get("a").map(String::as_str), Some("base"));
        assert_eq!(merged.get("b").map(String::as_str), Some("rendered"));
        assert_eq!(
            merged.get("shared").map(String::as_str),
            Some("rendered"),
            "rendered entries win on conflict"
        );
    }

    #[test]
    fn merge_metadata_with_empty_rendered_returns_base() {
        let base = meta(&[("a", "1")]);
        assert_eq!(merge_metadata(&base, &HashMap::new()), base);
    }
}
