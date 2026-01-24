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

use std::{
    collections::{btree_map::Entry, BTreeMap},
    hash::{Hash, Hasher},
    sync::Arc,
};

use hashers::fx_hash::FxHasher;
use ordered_float::OrderedFloat;

use crate::interface::IndexError;
use crate::{evaluation::variable_value::VariableValue, interface::ResultIndex};

#[derive(Clone)]
pub struct SortedSetEntryCount {
    pub snapshot: isize,
    pub delta: isize,
}

pub type SortedSetChangeLog = BTreeMap<OrderedFloat<f64>, SortedSetEntryCount>;

#[derive(Clone)]
pub struct LazySortedSet {
    set_id: u64,
    change_log: SortedSetChangeLog,
    store: Arc<dyn ResultIndex>, //todo: change to LazySortedSetStore after trait upcasting is supported by stable rust
}

impl LazySortedSet {
    pub fn new(
        position_in_query: usize,
        grouping_values: &Vec<VariableValue>,
        store: Arc<dyn ResultIndex>,
    ) -> LazySortedSet {
        let mut hasher = FxHasher::default();
        position_in_query.hash(&mut hasher);
        for value in grouping_values {
            value.to_string().hash(&mut hasher);
        }

        LazySortedSet {
            set_id: hasher.finish(),
            change_log: SortedSetChangeLog::new(),
            store,
        }
    }

    /// Returns the smallest value in the set that has a positive count.
    /// This function performs a proper two-way merge of the local change_log
    /// and the persisted storage, considering that:
    /// - Storage entries may have been deleted (delta < 0 in change_log)
    /// - New entries may exist only in change_log (not yet committed)
    /// - Both iterators are sorted in ascending order
    pub async fn get_head(&self) -> Result<Option<f64>, IndexError> {
        let mut storage_cursor = self.store.get_next(self.set_id, None).await?;
        let mut changelog_iter = self.change_log.iter().peekable();

        loop {
            match (changelog_iter.peek(), &storage_cursor) {
                // Both iterators exhausted
                (None, None) => return Ok(None),

                // Only storage values remain
                (None, Some((st_val, _st_count))) => {
                    // Check if this storage value was modified in change_log
                    if let Some(entry) = self.change_log.get(st_val) {
                        // Value exists in change_log, check effective count
                        if entry.snapshot + entry.delta > 0 {
                            return Ok(Some((*st_val).into()));
                        }
                        // Deleted in change_log, advance storage cursor
                    } else {
                        // No local changes for this value, it's valid
                        return Ok(Some((*st_val).into()));
                    }
                    storage_cursor = self.store.get_next(self.set_id, Some(*st_val)).await?;
                }

                // Only change_log values remain
                (Some(&(cl_val, cl_entry)), None) => {
                    if cl_entry.snapshot + cl_entry.delta > 0 {
                        return Ok(Some((*cl_val).into()));
                    }
                    changelog_iter.next();
                }

                // Both have values - merge them
                (Some(&(cl_val, cl_entry)), Some((st_val, _st_count))) => {
                    if cl_val < *st_val {
                        // Change_log entry is smaller
                        if cl_entry.snapshot + cl_entry.delta > 0 {
                            return Ok(Some((*cl_val).into()));
                        }
                        changelog_iter.next();
                    } else if cl_val == *st_val {
                        // Same value in both - use change_log's effective count
                        if cl_entry.snapshot + cl_entry.delta > 0 {
                            return Ok(Some((*cl_val).into()));
                        }
                        // Value was deleted, advance both
                        changelog_iter.next();
                        storage_cursor = self.store.get_next(self.set_id, Some(*st_val)).await?;
                    } else {
                        // Storage entry is smaller - check if it's been modified
                        if let Some(entry) = self.change_log.get(st_val) {
                            if entry.snapshot + entry.delta > 0 {
                                return Ok(Some((*st_val).into()));
                            }
                            // Deleted in change_log
                        } else {
                            // No local changes, storage value is valid
                            return Ok(Some((*st_val).into()));
                        }
                        storage_cursor = self.store.get_next(self.set_id, Some(*st_val)).await?;
                    }
                }
            }
        }
    }

    pub async fn insert(&mut self, value: f64) {
        match self.change_log.entry(value.into()) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().delta += 1;
            }
            Entry::Vacant(entry) => {
                let stored_count = self
                    .store
                    .get_value_count(self.set_id, value.into())
                    .await
                    .unwrap_or(0);
                entry.insert(SortedSetEntryCount {
                    snapshot: stored_count,
                    delta: 1,
                });
            }
        };
    }

    pub async fn remove(&mut self, value: f64) {
        match self.change_log.entry(value.into()) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().delta -= 1;
            }
            Entry::Vacant(entry) => {
                let stored_count = self
                    .store
                    .get_value_count(self.set_id, value.into())
                    .await
                    .unwrap_or(0);
                entry.insert(SortedSetEntryCount {
                    snapshot: stored_count,
                    delta: -1,
                });
            }
        };
    }

    pub async fn commit(&mut self) -> Result<(), IndexError> {
        for (value, entry) in &self.change_log {
            self.store
                .increment_value_count(self.set_id, *value, entry.delta)
                .await?;
        }
        self.change_log.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;

    /// Test: Empty set returns None
    #[tokio::test]
    async fn test_get_head_empty_set() {
        let store = Arc::new(InMemoryResultIndex::new());
        let set = LazySortedSet::new(0, &vec![], store);
        
        let result = set.get_head().await.unwrap();
        assert_eq!(result, None);
    }

    /// Test: Single insert returns that value
    #[tokio::test]
    async fn test_get_head_single_insert() {
        let store = Arc::new(InMemoryResultIndex::new());
        let mut set = LazySortedSet::new(0, &vec![], store);
        
        set.insert(5.0).await;
        
        let result = set.get_head().await.unwrap();
        assert_eq!(result, Some(5.0));
    }

    /// Test: Multiple inserts returns the smallest
    #[tokio::test]
    async fn test_get_head_multiple_inserts() {
        let store = Arc::new(InMemoryResultIndex::new());
        let mut set = LazySortedSet::new(0, &vec![], store);
        
        set.insert(10.0).await;
        set.insert(5.0).await;
        set.insert(15.0).await;
        
        let result = set.get_head().await.unwrap();
        assert_eq!(result, Some(5.0));
    }

    /// Test: Insert then remove the smallest, should return next smallest
    #[tokio::test]
    async fn test_get_head_insert_remove_smallest() {
        let store = Arc::new(InMemoryResultIndex::new());
        let mut set = LazySortedSet::new(0, &vec![], store);
        
        set.insert(5.0).await;
        set.insert(10.0).await;
        set.remove(5.0).await;
        
        let result = set.get_head().await.unwrap();
        assert_eq!(result, Some(10.0));
    }

    /// Test: Remove all items returns None
    #[tokio::test]
    async fn test_get_head_remove_all() {
        let store = Arc::new(InMemoryResultIndex::new());
        let mut set = LazySortedSet::new(0, &vec![], store);
        
        set.insert(5.0).await;
        set.remove(5.0).await;
        
        let result = set.get_head().await.unwrap();
        assert_eq!(result, None);
    }

    /// Test: Commit then query should return from storage
    #[tokio::test]
    async fn test_get_head_after_commit() {
        let store = Arc::new(InMemoryResultIndex::new());
        let mut set = LazySortedSet::new(0, &vec![], store.clone());
        
        set.insert(5.0).await;
        set.insert(10.0).await;
        set.commit().await.unwrap();
        
        // Create a new set pointing to the same store
        let set2 = LazySortedSet::new(0, &vec![], store);
        let result = set2.get_head().await.unwrap();
        assert_eq!(result, Some(5.0));
    }

    /// CRITICAL TEST: Delete from storage, add new smaller value in change_log
    /// This is the bug scenario: storage has [5], change_log has {5: delete, 3: add}
    /// Expected: 3 (since 5 is deleted and 3 is added)
    #[tokio::test]
    async fn test_get_head_delete_storage_add_smaller_changelog() {
        let store = Arc::new(InMemoryResultIndex::new());
        let mut set = LazySortedSet::new(0, &vec![], store.clone());
        
        // First, add 5 and commit to storage
        set.insert(5.0).await;
        set.commit().await.unwrap();
        
        // Now delete 5 and add 3 in the same transaction
        let mut set2 = LazySortedSet::new(0, &vec![], store);
        set2.remove(5.0).await;
        set2.insert(3.0).await;
        
        let result = set2.get_head().await.unwrap();
        assert_eq!(result, Some(3.0));
    }

    /// CRITICAL TEST: Storage has [1, 5], change_log has {1: delete, 3: add}
    /// Expected: 3 (since 1 is deleted, 3 < 5)
    #[tokio::test]
    async fn test_get_head_delete_smallest_storage_add_middle_changelog() {
        let store = Arc::new(InMemoryResultIndex::new());
        let mut set = LazySortedSet::new(0, &vec![], store.clone());
        
        // Commit 1 and 5 to storage
        set.insert(1.0).await;
        set.insert(5.0).await;
        set.commit().await.unwrap();
        
        // Delete 1, add 3
        let mut set2 = LazySortedSet::new(0, &vec![], store);
        set2.remove(1.0).await;
        set2.insert(3.0).await;
        
        let result = set2.get_head().await.unwrap();
        assert_eq!(result, Some(3.0));
    }

    /// Test: Storage has [2, 6], change_log has {4: add, 8: add}
    /// Expected: 2 (storage value is smaller and not deleted)
    #[tokio::test]
    async fn test_get_head_storage_smaller_than_changelog() {
        let store = Arc::new(InMemoryResultIndex::new());
        let mut set = LazySortedSet::new(0, &vec![], store.clone());
        
        // Commit 2 and 6 to storage
        set.insert(2.0).await;
        set.insert(6.0).await;
        set.commit().await.unwrap();
        
        // Add 4 and 8 in change_log
        let mut set2 = LazySortedSet::new(0, &vec![], store);
        set2.insert(4.0).await;
        set2.insert(8.0).await;
        
        let result = set2.get_head().await.unwrap();
        assert_eq!(result, Some(2.0));
    }

    /// Test: Interleaved values - storage [2, 4], change_log {3: add, 5: add}
    /// Expected: 2
    #[tokio::test]
    async fn test_get_head_interleaved_values() {
        let store = Arc::new(InMemoryResultIndex::new());
        let mut set = LazySortedSet::new(0, &vec![], store.clone());
        
        set.insert(2.0).await;
        set.insert(4.0).await;
        set.commit().await.unwrap();
        
        let mut set2 = LazySortedSet::new(0, &vec![], store);
        set2.insert(3.0).await;
        set2.insert(5.0).await;
        
        let result = set2.get_head().await.unwrap();
        assert_eq!(result, Some(2.0));
    }

    /// Test: Delete storage smallest, changelog has larger values
    /// Storage [1, 10], change_log {1: delete, 15: add}
    /// Expected: 10
    #[tokio::test]
    async fn test_get_head_delete_smallest_changelog_larger() {
        let store = Arc::new(InMemoryResultIndex::new());
        let mut set = LazySortedSet::new(0, &vec![], store.clone());
        
        set.insert(1.0).await;
        set.insert(10.0).await;
        set.commit().await.unwrap();
        
        let mut set2 = LazySortedSet::new(0, &vec![], store);
        set2.remove(1.0).await;
        set2.insert(15.0).await;
        
        let result = set2.get_head().await.unwrap();
        assert_eq!(result, Some(10.0));
    }

    /// Test: Multiple deletes from storage
    /// Storage [1, 2, 3], change_log {1: delete, 2: delete}
    /// Expected: 3
    #[tokio::test]
    async fn test_get_head_multiple_deletes() {
        let store = Arc::new(InMemoryResultIndex::new());
        let mut set = LazySortedSet::new(0, &vec![], store.clone());
        
        set.insert(1.0).await;
        set.insert(2.0).await;
        set.insert(3.0).await;
        set.commit().await.unwrap();
        
        let mut set2 = LazySortedSet::new(0, &vec![], store);
        set2.remove(1.0).await;
        set2.remove(2.0).await;
        
        let result = set2.get_head().await.unwrap();
        assert_eq!(result, Some(3.0));
    }

    /// Test: Duplicate value handling (insert same value twice)
    #[tokio::test]
    async fn test_get_head_duplicate_values() {
        let store = Arc::new(InMemoryResultIndex::new());
        let mut set = LazySortedSet::new(0, &vec![], store);
        
        set.insert(5.0).await;
        set.insert(5.0).await;
        set.remove(5.0).await;
        
        // Should still return 5.0 because we inserted twice and removed once
        let result = set.get_head().await.unwrap();
        assert_eq!(result, Some(5.0));
    }
}
