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

//! In-memory implementation of [`LiveResultsWriter`].
//!
//! Uses a `HashMap<u64, Vec<u8>>` per query to store live result rows.
//! Suitable for testing and volatile backends.

use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::interface::{IndexError, LiveResultsWriter, RowMutation};

/// In-memory live results writer for testing and volatile use.
///
/// Stores rows keyed by `(query_id, row_signature)`. Mutations apply
/// immediately (no transactional batching).
pub struct InMemoryLiveResultsWriter {
    data: RwLock<HashMap<String, HashMap<u64, Vec<u8>>>>,
}

impl Default for InMemoryLiveResultsWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryLiveResultsWriter {
    pub fn new() -> Self {
        Self {
            data: RwLock::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl LiveResultsWriter for InMemoryLiveResultsWriter {
    async fn apply_mutations(
        &self,
        query_id: &str,
        mutations: &[RowMutation<'_>],
    ) -> Result<(), IndexError> {
        let mut store = self.data.write().await;
        let rows = store.entry(query_id.to_string()).or_default();
        for m in mutations {
            match m.data {
                Some(data) => {
                    rows.insert(m.row_signature, data.to_vec());
                }
                None => {
                    rows.remove(&m.row_signature);
                }
            }
        }
        Ok(())
    }

    async fn read_snapshot(&self, query_id: &str) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        let store = self.data.read().await;
        let entries = match store.get(query_id) {
            Some(map) => map.iter().map(|(sig, data)| (*sig, data.clone())).collect(),
            None => Vec::new(),
        };
        Ok(entries)
    }

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        let mut store = self.data.write().await;
        store.remove(query_id);
        Ok(())
    }

    async fn row_count(&self, query_id: &str) -> Result<usize, IndexError> {
        let store = self.data.read().await;
        Ok(store.get(query_id).map_or(0, |m| m.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_apply_upserts() {
        let writer = InMemoryLiveResultsWriter::new();
        let mutations = vec![
            RowMutation {
                row_signature: 1,
                data: Some(b"row1"),
            },
            RowMutation {
                row_signature: 2,
                data: Some(b"row2"),
            },
        ];
        writer.apply_mutations("q1", &mutations).await.unwrap();

        assert_eq!(writer.row_count("q1").await.unwrap(), 2);
        let snapshot = writer.read_snapshot("q1").await.unwrap();
        assert_eq!(snapshot.len(), 2);
    }

    #[tokio::test]
    async fn test_apply_delete() {
        let writer = InMemoryLiveResultsWriter::new();
        writer
            .apply_mutations(
                "q1",
                &[RowMutation {
                    row_signature: 1,
                    data: Some(b"row1"),
                }],
            )
            .await
            .unwrap();

        writer
            .apply_mutations(
                "q1",
                &[RowMutation {
                    row_signature: 1,
                    data: None,
                }],
            )
            .await
            .unwrap();

        assert_eq!(writer.row_count("q1").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_upsert_overwrites() {
        let writer = InMemoryLiveResultsWriter::new();
        writer
            .apply_mutations(
                "q1",
                &[RowMutation {
                    row_signature: 1,
                    data: Some(b"v1"),
                }],
            )
            .await
            .unwrap();
        writer
            .apply_mutations(
                "q1",
                &[RowMutation {
                    row_signature: 1,
                    data: Some(b"v2"),
                }],
            )
            .await
            .unwrap();

        assert_eq!(writer.row_count("q1").await.unwrap(), 1);
        let snapshot = writer.read_snapshot("q1").await.unwrap();
        assert_eq!(snapshot[0].1, b"v2");
    }

    #[tokio::test]
    async fn test_clear() {
        let writer = InMemoryLiveResultsWriter::new();
        writer
            .apply_mutations(
                "q1",
                &[RowMutation {
                    row_signature: 1,
                    data: Some(b"data"),
                }],
            )
            .await
            .unwrap();
        writer.clear("q1").await.unwrap();
        assert_eq!(writer.row_count("q1").await.unwrap(), 0);
        assert!(writer.read_snapshot("q1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_isolation_between_queries() {
        let writer = InMemoryLiveResultsWriter::new();
        writer
            .apply_mutations(
                "q1",
                &[RowMutation {
                    row_signature: 1,
                    data: Some(b"a"),
                }],
            )
            .await
            .unwrap();
        writer
            .apply_mutations(
                "q2",
                &[RowMutation {
                    row_signature: 1,
                    data: Some(b"b"),
                }],
            )
            .await
            .unwrap();

        writer.clear("q1").await.unwrap();
        assert_eq!(writer.row_count("q1").await.unwrap(), 0);
        assert_eq!(writer.row_count("q2").await.unwrap(), 1);
    }
}
