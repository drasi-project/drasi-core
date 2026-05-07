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

//! In-memory implementation of [`CheckpointStore`].
//!
//! Used for volatile backends where no persistent storage is available.
//! `stage_checkpoint` applies immediately (no real transaction backing).
//! This store can be persisted across stop/start cycles within the same
//! process lifetime by holding a reference to it on the query object.

use std::collections::HashMap;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::RwLock;

use crate::interface::{CheckpointStore, IndexError, SourceCheckpoint};

/// Internal checkpoint state for a single source.
#[derive(Debug, Clone)]
struct CheckpointEntry {
    sequence: u64,
    source_position: Option<Bytes>,
}

/// In-memory checkpoint store for volatile backends.
///
/// Stores per-source checkpoints and an optional config hash in memory.
/// All operations apply immediately (no transactional staging).
pub struct InMemoryCheckpointStore {
    checkpoints: RwLock<HashMap<String, CheckpointEntry>>,
    config_hash: RwLock<Option<u64>>,
}

impl Default for InMemoryCheckpointStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryCheckpointStore {
    pub fn new() -> Self {
        Self {
            checkpoints: RwLock::new(HashMap::new()),
            config_hash: RwLock::new(None),
        }
    }
}

#[async_trait]
impl CheckpointStore for InMemoryCheckpointStore {
    fn is_persistent(&self) -> bool {
        false
    }

    async fn stage_checkpoint(
        &self,
        source_id: &str,
        sequence: u64,
        source_position: Option<&Bytes>,
    ) -> Result<(), IndexError> {
        let mut data = self.checkpoints.write().await;
        data.insert(
            source_id.to_string(),
            CheckpointEntry {
                sequence,
                source_position: source_position.cloned(),
            },
        );
        Ok(())
    }

    async fn read_checkpoint(
        &self,
        source_id: &str,
    ) -> Result<Option<SourceCheckpoint>, IndexError> {
        let data = self.checkpoints.read().await;
        Ok(data.get(source_id).map(|entry| SourceCheckpoint {
            sequence: entry.sequence,
            source_position: entry.source_position.clone(),
        }))
    }

    async fn read_all_checkpoints(&self) -> Result<HashMap<String, SourceCheckpoint>, IndexError> {
        let data = self.checkpoints.read().await;
        Ok(data
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    SourceCheckpoint {
                        sequence: v.sequence,
                        source_position: v.source_position.clone(),
                    },
                )
            })
            .collect())
    }

    async fn clear_checkpoints(&self) -> Result<(), IndexError> {
        let mut data = self.checkpoints.write().await;
        data.clear();
        let mut hash = self.config_hash.write().await;
        *hash = None;
        Ok(())
    }

    async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
        let mut data = self.config_hash.write().await;
        *data = Some(hash);
        Ok(())
    }

    async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
        let data = self.config_hash.read().await;
        Ok(*data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_checkpoint_round_trip() {
        let store = InMemoryCheckpointStore::new();

        // Initially empty
        assert!(store.read_checkpoint("src-1").await.unwrap().is_none());
        assert!(store.read_all_checkpoints().await.unwrap().is_empty());

        // Stage with position
        let pos = Bytes::from_static(&[1, 2, 3, 4, 5, 6, 7, 8]);
        store
            .stage_checkpoint("src-1", 5, Some(&pos))
            .await
            .unwrap();

        let cp = store.read_checkpoint("src-1").await.unwrap().unwrap();
        assert_eq!(cp.sequence, 5);
        assert_eq!(cp.source_position.as_ref(), Some(&pos));

        // Stage second source — first preserved
        let pos2 = Bytes::from_static(&[9, 10, 11, 12]);
        store
            .stage_checkpoint("src-2", 6, Some(&pos2))
            .await
            .unwrap();

        let all = store.read_all_checkpoints().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all["src-1"].sequence, 5);
        assert_eq!(all["src-2"].sequence, 6);
        assert_eq!(all["src-2"].source_position.as_ref(), Some(&pos2));

        // Stage with None position
        store.stage_checkpoint("src-1", 10, None).await.unwrap();
        let cp = store.read_checkpoint("src-1").await.unwrap().unwrap();
        assert_eq!(cp.sequence, 10);
        assert_eq!(cp.source_position, None);

        // Clear
        store.clear_checkpoints().await.unwrap();
        assert!(store.read_all_checkpoints().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_config_hash() {
        let store = InMemoryCheckpointStore::new();

        assert_eq!(store.read_config_hash().await.unwrap(), None);

        store.write_config_hash(12345).await.unwrap();
        assert_eq!(store.read_config_hash().await.unwrap(), Some(12345));

        store.write_config_hash(99999).await.unwrap();
        assert_eq!(store.read_config_hash().await.unwrap(), Some(99999));

        // Clear removes config hash too
        store.clear_checkpoints().await.unwrap();
        assert_eq!(store.read_config_hash().await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_large_source_position() {
        let store = InMemoryCheckpointStore::new();
        let big_pos = Bytes::from(vec![0xAB; 100]);
        store
            .stage_checkpoint("src-cosmos", 20, Some(&big_pos))
            .await
            .unwrap();
        let cp = store.read_checkpoint("src-cosmos").await.unwrap().unwrap();
        assert_eq!(cp.source_position.as_ref(), Some(&big_pos));
    }
}
