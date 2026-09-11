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
    cmp::Reverse,
    collections::{HashMap, HashSet},
};

use async_trait::async_trait;
use priority_queue::PriorityQueue;
use tokio::sync::RwLock;

use crate::{
    interface::{FutureElementRef, FutureQueue, IndexError, PushType},
    models::{ElementReference, ElementTimestamp},
};

#[derive(Default)]
struct FutureQueueState {
    queue: PriorityQueue<(usize, FutureElementRef), Reverse<u64>>,
    groups: HashMap<(usize, u64), HashSet<FutureElementRef>>,
}

impl FutureQueueState {
    fn remove_group(&mut self, position: usize, group: u64) {
        if let Some(entries) = self.groups.remove(&(position, group)) {
            for entry in entries {
                self.queue.remove(&(position, entry));
            }
        }
    }
}

#[derive(Default)]
pub struct InMemoryFutureQueue {
    data: RwLock<FutureQueueState>,
}

impl InMemoryFutureQueue {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl FutureQueue for InMemoryFutureQueue {
    async fn push(
        &self,
        push_type: PushType,
        position_in_query: usize,
        group_signature: u64,
        element_ref: &ElementReference,
        original_time: ElementTimestamp,
        due_time: ElementTimestamp,
    ) -> Result<bool, IndexError> {
        let mut data = self.data.write().await;
        let group = (position_in_query, group_signature);
        match push_type {
            PushType::IfNotExists if data.groups.contains_key(&group) => return Ok(false),
            PushType::Overwrite => data.remove_group(position_in_query, group_signature),
            PushType::Always | PushType::IfNotExists => {}
        }
        let entry = FutureElementRef {
            element_ref: element_ref.clone(),
            original_time,
            due_time,
            group_signature,
        };
        data.groups.entry(group).or_default().insert(entry.clone());
        data.queue
            .push((position_in_query, entry), Reverse(due_time));
        Ok(true)
    }

    async fn remove(
        &self,
        position_in_query: usize,
        group_signature: u64,
    ) -> Result<(), IndexError> {
        let mut data = self.data.write().await;
        data.remove_group(position_in_query, group_signature);
        Ok(())
    }

    async fn pop(&self) -> Result<Option<FutureElementRef>, IndexError> {
        let mut data = self.data.write().await;
        let Some(((position, entry), _)) = data.queue.pop() else {
            return Ok(None);
        };
        let group = (position, entry.group_signature);
        let entries = data
            .groups
            .get_mut(&group)
            .ok_or(IndexError::CorruptedData)?;
        if !entries.remove(&entry) {
            return Err(IndexError::CorruptedData);
        }
        if entries.is_empty() {
            data.groups.remove(&group);
        }
        Ok(Some(entry))
    }

    async fn peek_due_time(&self) -> Result<Option<u64>, IndexError> {
        Ok(self
            .data
            .read()
            .await
            .queue
            .peek()
            .map(|(_, Reverse(deadline))| *deadline))
    }

    async fn clear(&self) -> Result<(), IndexError> {
        *self.data.write().await = FutureQueueState::default();
        Ok(())
    }
}
