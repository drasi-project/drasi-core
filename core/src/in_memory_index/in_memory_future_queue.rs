use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use priority_queue::PriorityQueue;
use tokio::sync::RwLock;

use crate::{
    interface::{FutureElementRef, FutureQueue, IndexError, PushType},
    models::{ElementReference, ElementTimestamp},
};

struct FutureQueueState {
    // [(position_in_query, fut_element_ref)] -> due_time
    queue: PriorityQueue<(usize, FutureElementRef), i64>,

    // (position_in_query, group_signature) -> (due_time, (original_time, element_ref))
    map: HashMap<(usize, u64), BTreeMap<ElementTimestamp, (ElementTimestamp, ElementReference)>>,
}

pub struct InMemoryFutureQueue {
    data: RwLock<FutureQueueState>,
}

impl Default for InMemoryFutureQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryFutureQueue {
    pub fn new() -> Self {
        InMemoryFutureQueue {
            data: RwLock::new(FutureQueueState {
                queue: PriorityQueue::new(),
                map: HashMap::new(),
            }),
        }
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

        let should_push = match push_type {
            PushType::Always => true,
            PushType::IfNotExists => {
                match data.map.get_mut(&(position_in_query, group_signature)) {
                    Some(map) => map.is_empty(),
                    None => true,
                }
            }
            PushType::Overwrite => {
                match data.map.remove(&(position_in_query, group_signature)) {
                    Some(map) => {
                        for (due_time, (original_time, element_ref)) in map {
                            data.queue.remove(&(
                                position_in_query,
                                FutureElementRef {
                                    element_ref: element_ref.clone(),
                                    original_time,
                                    due_time,
                                    group_signature,
                                },
                            ));
                        }
                    }
                    None => {}
                };

                true
            }
        };

        if should_push {
            let fut_element_ref = FutureElementRef {
                element_ref: element_ref.clone(),
                original_time,
                due_time,
                group_signature,
            };
            data.queue
                .push((position_in_query, fut_element_ref), -(due_time as i64));

            data.map
                .entry((position_in_query, group_signature))
                .or_default()
                .insert(due_time, (original_time, element_ref.clone()));
        }

        Ok(should_push)
    }

    async fn remove(
        &self,
        position_in_query: usize,
        group_signature: u64,
    ) -> Result<(), IndexError> {
        let mut data = self.data.write().await;
        match data.map.remove(&(position_in_query, group_signature)) {
            Some(map) => {
                for (due_time, (original_time, element_ref)) in map {
                    data.queue.remove(&(
                        position_in_query,
                        FutureElementRef {
                            element_ref: element_ref.clone(),
                            original_time,
                            due_time,
                            group_signature,
                        },
                    ));
                }
            }
            None => {}
        };
        Ok(())
    }

    async fn pop(&self) -> Result<Option<FutureElementRef>, IndexError> {
        let mut data = self.data.write().await;
        match data.queue.pop() {
            Some((key, due_time)) => {
                let due_time = -due_time as u64;
                let map_key = (key.0, key.1.group_signature);
                match data.map.get_mut(&map_key) {
                    Some(map) => {
                        map.remove(&due_time);
                        if map.is_empty() {
                            data.map.remove(&map_key);
                        }
                    }
                    None => {}
                }
                Ok(Some(key.1))
            }
            None => Ok(None),
        }
    }

    async fn peek_due_time(&self) -> Result<Option<ElementTimestamp>, IndexError> {
        let data = self.data.read().await;
        match data.queue.peek() {
            Some((_, due_time)) => Ok(Some((due_time * -1) as u64)),
            None => Ok(None),
        }
    }

    async fn clear(&self) -> Result<(), IndexError> {
        let mut data = self.data.write().await;
        data.queue.clear();
        data.map.clear();
        Ok(())
    }
}
