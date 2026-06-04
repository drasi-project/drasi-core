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

//! In-memory implementation of [`OutboxWriter`].
//!
//! Uses a `BTreeMap<u64, Vec<u8>>` per query to store outbox entries in order.
//! Suitable for testing and volatile backends.

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::interface::{IndexError, OutboxWriter};

/// In-memory outbox writer for testing and volatile use.
///
/// Entries are stored in a `BTreeMap` keyed by sequence number, giving
/// efficient ordered access for `read_from` and `trim_to_capacity`.
pub struct InMemoryOutboxWriter {
    data: RwLock<HashMap<String, BTreeMap<u64, Vec<u8>>>>,
}

impl Default for InMemoryOutboxWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryOutboxWriter {
    pub fn new() -> Self {
        Self {
            data: RwLock::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl OutboxWriter for InMemoryOutboxWriter {
    async fn append(&self, query_id: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError> {
        let mut store = self.data.write().await;
        store
            .entry(query_id.to_string())
            .or_default()
            .insert(sequence, data.to_vec());
        Ok(())
    }

    async fn read_from(
        &self,
        query_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        let store = self.data.read().await;
        let entries = match store.get(query_id) {
            Some(map) => map
                .range((
                    std::ops::Bound::Excluded(after_sequence),
                    std::ops::Bound::Unbounded,
                ))
                .map(|(seq, data)| (*seq, data.clone()))
                .collect(),
            None => Vec::new(),
        };
        Ok(entries)
    }

    async fn read_latest_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        let store = self.data.read().await;
        Ok(store
            .get(query_id)
            .and_then(|map| map.keys().next_back().copied()))
    }

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        let mut store = self.data.write().await;
        store.remove(query_id);
        Ok(())
    }

    async fn trim_to_capacity(&self, query_id: &str, capacity: usize) -> Result<usize, IndexError> {
        let mut store = self.data.write().await;
        let map = match store.get_mut(query_id) {
            Some(map) => map,
            None => return Ok(0),
        };
        let len = map.len();
        if len <= capacity {
            return Ok(0);
        }
        let to_remove = len - capacity;
        let keys_to_remove: Vec<u64> = map.keys().take(to_remove).copied().collect();
        for key in &keys_to_remove {
            map.remove(key);
        }
        Ok(to_remove)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_append_and_read() {
        let writer = InMemoryOutboxWriter::new();
        writer.append("q1", 1, b"hello").await.unwrap();
        writer.append("q1", 2, b"world").await.unwrap();
        writer.append("q1", 5, b"skip").await.unwrap();

        let entries = writer.read_from("q1", 0).await.unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0], (1, b"hello".to_vec()));
        assert_eq!(entries[1], (2, b"world".to_vec()));
        assert_eq!(entries[2], (5, b"skip".to_vec()));

        let entries = writer.read_from("q1", 2).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], (5, b"skip".to_vec()));
    }

    #[tokio::test]
    async fn test_read_latest_sequence() {
        let writer = InMemoryOutboxWriter::new();
        assert_eq!(writer.read_latest_sequence("q1").await.unwrap(), None);

        writer.append("q1", 10, b"data").await.unwrap();
        writer.append("q1", 20, b"data").await.unwrap();
        assert_eq!(writer.read_latest_sequence("q1").await.unwrap(), Some(20));
    }

    #[tokio::test]
    async fn test_clear() {
        let writer = InMemoryOutboxWriter::new();
        writer.append("q1", 1, b"data").await.unwrap();
        writer.clear("q1").await.unwrap();
        let entries = writer.read_from("q1", 0).await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn test_trim_to_capacity() {
        let writer = InMemoryOutboxWriter::new();
        for i in 1..=10 {
            writer.append("q1", i, b"data").await.unwrap();
        }
        let removed = writer.trim_to_capacity("q1", 3).await.unwrap();
        assert_eq!(removed, 7);

        let entries = writer.read_from("q1", 0).await.unwrap();
        assert_eq!(entries.len(), 3);
        // Should keep the latest 3: 8, 9, 10
        assert_eq!(entries[0].0, 8);
        assert_eq!(entries[1].0, 9);
        assert_eq!(entries[2].0, 10);
    }

    #[tokio::test]
    async fn test_trim_no_op_when_under_capacity() {
        let writer = InMemoryOutboxWriter::new();
        writer.append("q1", 1, b"data").await.unwrap();
        let removed = writer.trim_to_capacity("q1", 5).await.unwrap();
        assert_eq!(removed, 0);
    }

    #[tokio::test]
    async fn test_isolation_between_queries() {
        let writer = InMemoryOutboxWriter::new();
        writer.append("q1", 1, b"a").await.unwrap();
        writer.append("q2", 1, b"b").await.unwrap();

        writer.clear("q1").await.unwrap();
        assert!(writer.read_from("q1", 0).await.unwrap().is_empty());
        assert_eq!(writer.read_from("q2", 0).await.unwrap().len(), 1);
    }
}
